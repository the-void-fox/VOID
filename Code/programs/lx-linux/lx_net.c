/* lx_net.c — сетевой рантайм/заглушки Lx_kit (Веха 68), НЕ исходник Linux.
 *
 * «generated_dummies»-слой (приём Genode dde_linux): даёт ТЕЛА сетевым символам, на которые
 * ссылается неизменённый e1000 (netdev/skb/dma/napi/irq/страницы), поверх примитивов VOID. Часть —
 * настоящие (аллокация netdev/skb/dma через кучу и kmalloc), часть — заглушки-no-op под пути, что
 * оживут на следующей вехе (probe/open/TX/RX/ISR против реального QEMU-e1000 по MMIO/DMA/IRQ-cap).
 * Сейчас харнесс (main_e1000.c) зовёт лишь ЧИСТУЮ логику e1000_hw.c — этот слой нужен для ЛИНКОВКИ.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <linux/dma-mapping.h>
#include <linux/etherdevice.h>
#include <linux/random.h>
#include <linux/interrupt.h>
#include <linux/kernel.h>
#include <linux/mm.h>
#include <linux/netdevice.h>
#include <linux/ethtool.h>
#include <linux/mii.h>
#include <linux/pci.h>
#include <linux/skbuff.h>
#include <linux/slab.h>
#include <linux/string.h>

#include "lx_sched.h" /* lx_irq_register — request_irq заводит IRQ в планировщике (Веха 72) */

/* Настоящий DMA поверх DMA-cap VOID (SYS_DMA_ALLOC) — только в сборке драйвера (-DLX_HAVE_SYSCALL,
 * -I${void-libc}/lib). В сборке-«вычислялке» (Веха 68) DMA не зовётся → остаётся куча-версия. */
#ifdef LX_HAVE_SYSCALL
#include <syscall.h> /* vsys_dma_alloc / VOID_NO_CAP */
static uintptr_t lx_dma_cap    = VOID_NO_CAP;
static uintptr_t lx_dma_va_next = 0x58000000UL; /* DMA_BASE, как в lx_emul (Веха 54) */
void lx_net_set_dma_cap(uintptr_t cap) { lx_dma_cap = cap; }

/* IRQ-cap драйвера (start_cap 2): request_irq регистрирует им обработчик в планировщике. */
static uintptr_t lx_irq_cap = VOID_NO_CAP;
void lx_net_set_irq_cap(uintptr_t cap) { lx_irq_cap = cap; }
#endif

/* ─── АРЕНА DMA под буферы пакетов (Веха 133) ────────────────────────────────
 *
 * Зачем она. `dma_map_single` обязан отдать адрес, ПО КОТОРОМУ ХОДИТ КАРТА, — физический. У нас
 * он возвращал виртуальный, и это работало ровно до тех пор, пока никто не пробовал ПРИНИМАТЬ
 * пакеты: карта писала бы DMA'ом по случайной физической памяти, а выглядело бы это как порча
 * чужих данных — самое дорогое в поиске.
 *
 * Переводить произвольный VA в физический мы не можем и не хотим: это потребовало бы от ядра
 * нового полномочия «назови физический адрес любой моей страницы». Вместо этого буферы пакетов
 * с самого начала берутся ИЗ ОБЛАСТИ, полученной по DMA-праву. Тогда перевод — арифметика в её
 * пределах, а для памяти не из арены `dma_map_single` честно отказывает.
 *
 * Возврат (Веха 134). Сперва арена была одноразовой: буферы приёма живут столько же, сколько
 * драйвер, и этого хватало. Но вещатель журнала берёт буфер НА КАЖДЫЙ КАДР — без возврата арена
 * кончилась бы через несколько сотен кадров, и передача прекратилась бы ТИХО (кадр без адреса
 * карте не отдать). Поэтому freed-блоки складываются в табличку и переиспользуются.
 *
 * Табличка, а не настоящий аллокатор, — сознательно: блоки здесь одного-двух размеров (буфер
 * приёма и кадр журнала), поэтому первый подходящий находится сразу, а дробить и склеивать
 * нечего. Не нашлось подходящего — берём из нетронутого хвоста. Кончилось всё — скажем вслух,
 * а не отдадим невалидный адрес молча.
 */
/* 2 МиБ. Кольцо приёма atl1c — 512 буферов, и при MTU 1500 это ровно мегабайт: впритык к
 * прежнему размеру арены, то есть лишняя переменная при отладке. Запас вдвое стоит двух мегабайт
 * физической памяти на машине, где их четыре тысячи. */
#define LX_DMA_ARENA_PAGES 512

/* Веха 199.19 — возвращённые области `dma_alloc_coherent`. Колец у карты немного (кольцо
 * дескрипторов и его спутники), поэтому восьми записей хватает с запасом. */
#define LX_COH_MAX 8
static struct {
	uintptr_t va;
	uintptr_t pa;
	size_t    pages;
} lx_coh[LX_COH_MAX];
static unsigned lx_coh_n;

static uintptr_t lx_arena_va;   /* начало арены в нашем пространстве */
static uintptr_t lx_arena_pa;   /* её же физический адрес — карта ходит сюда */
static size_t    lx_arena_size;
static size_t    lx_arena_used;
static int       lx_arena_failed;

/* Возвращённые блоки: первый подходящий по размеру берётся заново (см. описание выше). */
#define LX_ARENA_FREE_MAX 2048
static struct {
	void  *p;
	size_t size;
} lx_arena_free[LX_ARENA_FREE_MAX];
static unsigned lx_arena_free_n;

/* Отдать `size` байт из арены (выравнивание на 8). NULL — арены нет или она кончилась. */
static void *lx_arena_alloc(size_t size)
{
#ifdef LX_HAVE_SYSCALL
	size_t need = (size + 7) & ~(size_t)7;
	unsigned i;

	if (!lx_arena_va && !lx_arena_failed && lx_dma_cap != VOID_NO_CAP) {
		uintptr_t va = lx_dma_va_next;
		uintptr_t pa = vsys_dma_alloc_n(lx_dma_cap, va, LX_DMA_ARENA_PAGES);
		if (pa == VOID_NO_CAP) {
			lx_arena_failed = 1;
			printk("lx_net: арена DMA не завелась — приём работать не будет\n");
		} else {
			lx_dma_va_next += (uintptr_t)LX_DMA_ARENA_PAGES * 4096;
			lx_arena_va = va;
			lx_arena_pa = pa;
			lx_arena_size = (size_t)LX_DMA_ARENA_PAGES * 4096;
			/* Веха 199.7 — ФИЗИЧЕСКИЙ адрес вслух. Это единственное число во всей цепочке
			 * приёма, которое мы называем карте и проверить не можем ничем, кроме как
			 * прочитав его же обратно. Карты семейства atl1c объявляют 32-битный DMA — если
			 * арена окажется выше 4 ГиБ, старшая половина адреса просто не доедет, и приём
			 * умрёт молча. Строка стоит одной; догадка о ней стоила бы перезагрузки. */
			printk("lx_net: арена DMA %u КиБ: VA %lx → физ %lx\n",
			       (unsigned)(lx_arena_size / 1024),
			       (unsigned long)lx_arena_va, (unsigned long)lx_arena_pa);
		}
	}
	/* Сперва — из возвращённых: без этого вечный вещатель журнала съел бы арену за минуты. */
	for (i = 0; i < lx_arena_free_n; i++) {
		if (lx_arena_free[i].size >= need) {
			void *p = lx_arena_free[i].p;

			lx_arena_free[i] = lx_arena_free[--lx_arena_free_n];
			return p;
		}
	}
	if (lx_arena_va && lx_arena_used + need <= lx_arena_size) {
		void *p = (void *)(lx_arena_va + lx_arena_used);
		lx_arena_used += need;
		return p;
	}
	if (lx_arena_va) {
		printk("lx_net: арена DMA кончилась (%u из %u байт, в возврате %u блоков)\n",
		       (unsigned)lx_arena_used, (unsigned)lx_arena_size, lx_arena_free_n);
	}
#else
	(void)size; /* сборка-«вычислялка»: DMA не задействован, буферы идут из кучи */
#endif
	return NULL;
}

/* Вернуть блок арене. Не влез в табличку — блок просто теряется, и об этом говорим: молчаливая
 * утечка кончилась бы «передача сама собой прекратилась через полчаса». */
static void lx_arena_free_block(void *p, size_t size)
{
	if (lx_arena_free_n < LX_ARENA_FREE_MAX) {
		lx_arena_free[lx_arena_free_n].p = p;
		lx_arena_free[lx_arena_free_n].size = (size + 7) & ~(size_t)7;
		lx_arena_free_n++;
		return;
	}
	printk("lx_net: табличка возврата арены полна — блок %u байт потерян\n", (unsigned)size);
}

/* Лежит ли указатель в арене — то есть можно ли назвать карте его адрес. */
static int lx_in_arena(const void *p)
{
	uintptr_t a = (uintptr_t)p;
	return lx_arena_va && a >= lx_arena_va && a < lx_arena_va + lx_arena_size;
}

/* ─ состояние системы (e1000 shutdown отличает выключение) ─ */
enum system_states system_state = SYSTEM_RUNNING;

/* ─ строки ─ */
ssize_t strscpy(char *dst, const char *src, size_t size)
{
	size_t i = 0;
	if (!size) return -7 /* -E2BIG */;
	for (; i < size - 1 && src[i]; i++) dst[i] = src[i];
	dst[i] = '\0';
	return src[i] ? -7 : (ssize_t)i;
}

void print_hex_dump(const char *level, const char *prefix, int ptype, int rowsize,
		    int groupsize, const void *buf, size_t len, bool ascii)
{ (void)level; (void)prefix; (void)ptype; (void)rowsize; (void)groupsize; (void)buf; (void)len; (void)ascii; }

/* ─ страницы (страница = кусок кучи; struct page* несёт сам адрес) ─ */
struct page *alloc_pages(gfp_t gfp, unsigned int order)
{ (void)gfp; return (struct page *)kmalloc(PAGE_SIZE << order, 0); }
void __free_pages(struct page *page, unsigned int order) { (void)order; kfree(page); }
void *page_address(const struct page *page) { return (void *)page; }
struct page *virt_to_page(const void *addr) { return (struct page *)addr; }
void  get_page(struct page *page) { (void)page; }
void  put_page(struct page *page) { (void)page; }
phys_addr_t page_to_phys(const struct page *page) { return (phys_addr_t)page; }

/* ─ vmalloc (единая куча newlib) ─ */
void *vmalloc(unsigned long size) { return malloc(size); }
void *vzalloc(unsigned long size) { void *p = malloc(size); if (p) memset(p, 0, size); return p; }
void  vfree(const void *addr) { free((void *)addr); }

/* ─ DMA: физически-непрерывная память, phys == адрес для устройства (настоящий DMA — по DMA-cap) ─ */
void *dma_alloc_coherent(struct device *dev, size_t size, dma_addr_t *handle, gfp_t gfp)
{
	void *p;
	(void)dev; (void)gfp;
	/* Веха 133.3 — СЛЕД подъёма. Драйвер повис где-то внутри ndo_open и не сказал ни слова:
	 * вендорный код печатает только об ошибках, а «дошёл сюда» в нём нет. Трогать его нельзя —
	 * значит говорить обязаны наши шимы, через которые он и ходит. Дёшево (несколько строк на
	 * подъём) и отвечает на главный вопрос: докуда добрался. */
	printk("lx_net: dma_alloc_coherent(%u байт)\n", (unsigned)size);
#ifdef LX_HAVE_SYSCALL
	/* Реальный DMA: `pages` ПОДРЯД идущих страниц по DMA-cap; физ-адрес начала — device-адрес.
	 *
	 * Веха 133 — здесь стоял потолок в одну страницу, а всё, что больше, тихо уходило в кучу:
	 * `*handle` получал тогда ВИРТУАЛЬНЫЙ адрес, и карта писала бы DMA'ом по случайной физической
	 * памяти. Пока драйверам хватало страницы, это не всплывало; кольцам atl1c нужны десятки
	 * килобайт — всплыло бы сразу, порчей чужой памяти. */
	if (lx_dma_cap != VOID_NO_CAP) {
		size_t pages = (size + 4095) / 4096;
		uintptr_t va;
		uintptr_t pa;
		unsigned i;

		/* Веха 199.19 — СПЕРВА ИЗ ВОЗВРАЩЁННЫХ. Перезапуск интерфейса (`ndo_stop` + `ndo_open`,
		 * им лечится застрявший приём — Веха 199.18) освобождает кольца и просит их заново. А
		 * вернуть страницы ядру мы не умеем: права «отдать DMA обратно» нет, и заводить его ради
		 * этого незачем — область той же длины нужна тому же драйверу через миг. Без
		 * переиспользования каждый перезапуск съедал бы по сорок пять килобайт DMA навсегда,
		 * и лечение медленно превращалось бы в новую болезнь. */
		for (i = 0; i < lx_coh_n; i++) {
			if (lx_coh[i].pages >= pages && lx_coh[i].va) {
				va = lx_coh[i].va;
				pa = lx_coh[i].pa;
				lx_coh[i] = lx_coh[--lx_coh_n];
				memset((void *)va, 0, size);
				*handle = (dma_addr_t)pa;
				printk("lx_net:   кольца легли по физ %lx (область переиспользована)\n",
				       (unsigned long)pa);
				return (void *)va;
			}
		}
		va = lx_dma_va_next;
		pa = vsys_dma_alloc_n(lx_dma_cap, va, pages);
		if (pa == VOID_NO_CAP) { *handle = 0; return NULL; }
		lx_dma_va_next += pages * 4096;
		memset((void *)va, 0, size);
		*handle = (dma_addr_t)pa;
		printk("lx_net:   кольца легли по физ %lx\n", (unsigned long)pa);
		return (void *)va;
	}
#endif
	p = kmalloc(size, 0); /* «вычислялка» (Веха 68): DMA не задействован — куча */
	if (p) memset(p, 0, size);
	*handle = (dma_addr_t)(unsigned long)p;
	return p;
}
void dma_free_coherent(struct device *dev, size_t size, void *vaddr, dma_addr_t handle)
{
	(void)dev;
#ifdef LX_HAVE_SYSCALL
	if (lx_dma_cap != VOID_NO_CAP) {
		/* Ядру страницы не возвращаем (права на это нет), но помним их за собой — следующий
		 * `dma_alloc_coherent` той же длины заберёт эту же область (см. Веху 199.19 выше).
		 * Не влезло в табличку — область просто теряется, и об этом говорим: молчаливая утечка
		 * DMA кончилась бы «сеть перестала подниматься после N перезапусков». */
		if (lx_coh_n < LX_COH_MAX) {
			lx_coh[lx_coh_n].va = (uintptr_t)vaddr;
			lx_coh[lx_coh_n].pa = (uintptr_t)handle;
			lx_coh[lx_coh_n].pages = (size + 4095) / 4096;
			lx_coh_n++;
		} else {
			printk("lx_net: некуда отложить область DMA (%u байт) — потеряна\n", (unsigned)size);
		}
		return;
	}
#endif
	(void)size; (void)handle;
	kfree(vaddr);
}
/* Веха 133 — адрес ДЛЯ КАРТЫ. Работает только для памяти из арены DMA; для всего прочего
 * возвращает 0, и вызывающий обязан это заметить (`dma_mapping_error`). Соврать здесь
 * виртуальным адресом, как было раньше, значит отправить карту писать по случайной физической
 * памяти — ошибка, которая проявится далеко от места и не в этом драйвере. */
dma_addr_t dma_map_single(struct device *dev, void *ptr, size_t size, int dir)
{
	(void)dev; (void)size; (void)dir;
	if (!lx_in_arena(ptr)) {
		printk("lx_net: dma_map_single вне арены DMA — адрес карте не назвать\n");
		return 0;
	}
	return (dma_addr_t)(lx_arena_pa + ((uintptr_t)ptr - lx_arena_va));
}
void dma_unmap_single(struct device *dev, dma_addr_t addr, size_t size, int dir)
{ (void)dev; (void)addr; (void)size; (void)dir; }
dma_addr_t dma_map_page(struct device *dev, struct page *page, size_t offset, size_t size, int dir)
{ (void)dev; (void)size; (void)dir; return (dma_addr_t)(unsigned long)page + offset; }
void dma_unmap_page(struct device *dev, dma_addr_t addr, size_t size, int dir)
{ (void)dev; (void)addr; (void)size; (void)dir; }
int  dma_mapping_error(struct device *dev, dma_addr_t addr) { (void)dev; return addr == 0; }
int  dma_set_mask(struct device *dev, u64 mask) { (void)dev; (void)mask; return 0; }
int  dma_set_coherent_mask(struct device *dev, u64 mask) { (void)dev; (void)mask; return 0; }
int  dma_set_mask_and_coherent(struct device *dev, u64 mask) { (void)dev; (void)mask; return 0; }
void dma_sync_single_for_cpu(struct device *dev, dma_addr_t a, size_t s, int d) { (void)dev; (void)a; (void)s; (void)d; }
void dma_sync_single_for_device(struct device *dev, dma_addr_t a, size_t s, int d) { (void)dev; (void)a; (void)s; (void)d; }

/* ─ netdev: аллокация/регистрация ─ */
struct net_device *alloc_etherdev_mq(int sizeof_priv, unsigned int txqs)
{
	struct net_device *dev;

	/* Больше одной очереди мы не умеем — и говорим об этом отказом, а не молчанием.
	 * Драйвер, попросивший четыре и получивший одну, разложил бы кольца по четырём наборам
	 * регистров, а будил бы одну очередь: пакеты уходили бы в три молчащих кольца, и выглядело
	 * бы это как «иногда теряются пакеты» — худший вид ошибки. Веха 131. */
	if (txqs != 1) {
		printk("lx_net: alloc_etherdev_mq(%u очередей) — умеем только одну\n", txqs);
		return NULL;
	}

	dev = kmalloc(sizeof(*dev), 0);
	if (!dev) return NULL;
	memset(dev, 0, sizeof(*dev));
	dev->lx_priv = kmalloc(sizeof_priv, 0);
	if (dev->lx_priv) memset(dev->lx_priv, 0, sizeof_priv);
	/* Веха 133.4 — ГОЛОВЫ СПИСКОВ обязаны указывать САМИ НА СЕБЯ. Обнулённый `list_head` это не
	 * пустой список, а битый: `next == NULL`, и первый же обход (`netdev_for_each_mc_addr` в
	 * `atl1c_set_multi`) уходит по нулевому адресу. Здесь стоял только memset — и подъём
	 * интерфейса вставал намертво ровно на этом месте, молча.
	 *
	 * Ошибка пряталась потому, что до atl1c списки НИКТО НЕ ОБХОДИЛ: e1000-харнессы до
	 * set_rx_mode не доходили. Первый же настоящий драйвер, дошедший до настройки фильтров,
	 * наступил на неё сразу. */
	INIT_LIST_HEAD(&dev->mc.list);
	INIT_LIST_HEAD(&dev->uc.list);
	dev->mc.count = 0; dev->uc.count = 0;

	/* ── Веха 199.7 — `ether_setup`: то, что в Linux делает `alloc_etherdev` ВНУТРИ СЕБЯ ──
	 *
	 * Здесь стоял один `memset`, и поле `mtu` оставалось НУЛЁМ. Драйвер имеет полное право
	 * считать, что размер кадра ему уже назвали: в Linux `alloc_etherdev` зовёт `ether_setup`,
	 * а тот ставит 1500 ДО того, как драйвер впервые заглянет в `netdev->mtu`.
	 *
	 * Чего это стоило. atl1c берёт оттуда `hw->max_frame_size` и программирует им РЕГИСТР КАРТЫ:
	 *
	 *     AT_WRITE_REG(hw, REG_MTU, hw->max_frame_size + ETH_HLEN + VLAN_HLEN + ETH_FCS_LEN);
	 *
	 * То есть карте говорилось: «кадры длиннее ДВАДЦАТИ ДВУХ байт не принимай». Самый короткий
	 * Ethernet-кадр — шестьдесят. Карта честно выбрасывала ВСЁ, что приходило с провода, и столь
	 * же честно молчала об этом: отброшенный по длине кадр не поднимает прерывания и не пишет
	 * дескриптор. Снаружи это выглядело как «линк есть, кадр уходит, в ответ тишина» — и увело
	 * поиск в прерывания, маршрутизацию линий и DMA, где всё было исправно.
	 *
	 * Почему не всплыло в QEMU: ни rtl8139, ни e1000 не программируют картe предел длины из
	 * `netdev->mtu` — у первого его нет вовсе, у второго модель QEMU его не проверяет. Ошибка
	 * ждала первой карты, которая этому полю верит.
	 *
	 * Мораль та же, что у голов списков выше: обнулённая структура — это не «пустая», а
	 * НЕПРАВИЛЬНО ЗАПОЛНЕННАЯ, и ноль в ней значит ровно ноль, а не «по умолчанию».
	 */
	dev->mtu      = ETH_DATA_LEN;          /* 1500 — как ether_setup */
	dev->min_mtu  = 68;                    /* ETH_MIN_MTU */
	dev->max_mtu  = ETH_DATA_LEN;          /* драйвер поднимет сам, если умеет jumbo */
	dev->addr_len = ETH_ALEN;
	dev->flags    = IFF_BROADCAST | IFF_MULTICAST;

	dev->lx_txq = kmalloc(sizeof(*dev->lx_txq), 0);
	if (!dev->lx_txq) { kfree(dev->lx_priv); kfree(dev); return NULL; }
	dev->lx_txq->dev = dev;
	dev->lx_num_tx_queues = txqs;
	return dev;
}

struct net_device *alloc_etherdev(int sizeof_priv)
{
	return alloc_etherdev_mq(sizeof_priv, 1);
}

void free_netdev(struct net_device *dev)
{ if (dev) { kfree(dev->lx_txq); kfree(dev->lx_priv); kfree(dev); } }
/* Веха 133.2 — ИМЯ интерфейса. Драйвер кладёт в `name` шаблон «eth%d», а номер подставляет
 * ядро при регистрации; у нас этого не делал никто, и в лог уходило буквальное «ethN».
 * Интерфейс у нас пока один, поэтому номер всегда 0 — но подставлять его обязан тот, кто
 * регистрирует, иначе имя в логе и имя в системе разойдутся при первом же втором устройстве. */
int register_netdev(struct net_device *dev)
{
	static int next_index;
	char *pc = strchr(dev->name, '%');

	if (pc && pc[1] == 'd') {
		int n = snprintf(pc, sizeof(dev->name) - (size_t)(pc - dev->name), "%d", next_index++);
		(void)n;
	} else if (!dev->name[0]) {
		snprintf(dev->name, sizeof(dev->name), "eth%d", next_index++);
	}
	printk("lx_net: register_netdev('%s')\n", dev->name);
	return 0;
}
void unregister_netdev(struct net_device *dev) { (void)dev; }

__be16 eth_type_trans(struct sk_buff *skb, struct net_device *dev)
{ (void)dev; return skb ? skb->protocol : 0; }
int eth_validate_addr(struct net_device *dev)
{ return is_valid_ether_addr(dev->dev_addr) ? 0 : -22 /* -EINVAL */; }
void eth_random_addr(u8 *addr)
{
	get_random_bytes(addr, ETH_ALEN);
	addr[0] &= 0xfe; /* одноадресный */
	addr[0] |= 0x02; /* назначен локально */
}

void eth_hw_addr_random(struct net_device *dev) { eth_random_addr(dev->dev_addr); }

/* Веха 133 — ethtool СОЗНАТЕЛЬНО отсутствует. `atl1c_ethtool.c` в сборку не входит: это
 * интерфейс для одноимённой утилиты Linux, которой у нас нет, а тянет он за собой три десятка
 * структур (link_ksettings, drvinfo, regs, wolinfo…), к работе карты отношения не имеющих.
 * Драйвер зовёт это в probe безусловно, поэтому пустое тело обязано быть: netdev остаётся без
 * ethtool_ops, как устройство, которое ethtool не поддерживает. Понадобится показывать состояние
 * линка в сетевом TUI — шим вырастет тогда, под настоящего потребителя. */
void atl1c_set_ethtool_ops(struct net_device *netdev) { (void)netdev; }

struct netdev_queue *netdev_get_tx_queue(struct net_device *dev, unsigned int index)
{
	(void)index; /* очередь одна — см. alloc_etherdev_mq */
	return dev->lx_txq;
}

/* Остановка/пробуждение ОЧЕРЕДИ сводятся к устройству: очередь у нас одна, и её состояние и
 * есть состояние устройства. */
void netif_tx_stop_queue(struct netdev_queue *q)        { netif_stop_queue(q->dev); }
void netif_tx_wake_queue(struct netdev_queue *q)        { netif_wake_queue(q->dev); }
bool netif_tx_queue_stopped(const struct netdev_queue *q) { return netif_queue_stopped(q->dev); }

/* Пересчёт активных фич: у нас их выставляет драйвер и никто не оспаривает. */
void netdev_update_features(struct net_device *dev) { (void)dev; }

/* NAPI передачи — тот же кооперативный планировщик, что и у приёма (в Linux это разные
 * контексты, у нас один). Отдельная нить опроса (`netif_threaded_enable`) не нужна по той же
 * причине: задачи Lx_kit и так уступают процессор друг другу. */
void netif_napi_add_tx(struct net_device *dev, struct napi_struct *napi,
		       int (*poll)(struct napi_struct *, int))
{
	netif_napi_add(dev, napi, poll);
}

int netif_threaded_enable(struct net_device *dev) { (void)dev; return 0; }

/* ─ netif_* очереди/несущая (оживут при open/link на след. вехе) ─ */
void netif_start_queue(struct net_device *dev)
{ (void)dev; printk("lx_net: netif_start_queue — очередь передачи открыта\n"); }
void netif_stop_queue(struct net_device *dev) { (void)dev; }
void netif_wake_queue(struct net_device *dev) { (void)dev; }
void netif_tx_disable(struct net_device *dev) { (void)dev; }
bool netif_queue_stopped(const struct net_device *dev) { (void)dev; return false; }
bool netif_running(const struct net_device *dev) { return dev->flags & IFF_UP; }
/* Веха 133.3 — след подъёма: несущая переключается в начале `atl1c_up` и по результату
 * согласования, то есть по этим двум строкам видно, дошёл ли драйвер до работы с линком. */
void netif_carrier_on(struct net_device *dev)
{
	dev->lx_state |= 1UL << LX_STATE_CARRIER;
	printk("lx_net: netif_carrier_on — несущая есть\n");
}
void netif_carrier_off(struct net_device *dev)
{
	dev->lx_state &= ~(1UL << LX_STATE_CARRIER);
	printk("lx_net: netif_carrier_off\n");
}
/* Веха 134.3 — состояние несущей ВЕДЁТСЯ, а не выдумывается. Здесь стояло `return true`, и
 * харнесс бодро печатал «несущая ЕСТЬ» в прогоне, где `netif_carrier_on` не звался ни разу.
 * Заглушка, отвечающая «да» независимо от происходящего, — это не упрощение, а ложный свидетель. */
bool netif_carrier_ok(const struct net_device *dev)
{
	return (dev->lx_state & (1UL << LX_STATE_CARRIER)) != 0;
}
void netif_device_attach(struct net_device *dev) { (void)dev; }
void netif_device_detach(struct net_device *dev) { (void)dev; }

/* ─── NAPI — НАСТОЯЩИЙ (Веха 195) ────────────────────────────────────────────
 *
 * Здесь стояли заглушки, и именно они были причиной, по которой хостируемый драйвер не принимал
 * ни одного кадра: `napi_schedule_prep` отвечала «нет», обработчик прерывания послушно не
 * планировал опрос, и `poll` — та функция, что вынимает кадры из кольца, — не звалась НИКОГДА.
 * Снаружи это выглядело как «драйвер работает, кадров нет», то есть хуже всякой ошибки.
 *
 * В Linux опрос идёт в softirq (`net_rx_action`). У нас есть ровно такой контекст —
 * холостой путь планировщика Lx_kit, который уже зовёт обработчики прерываний. Оттуда и
 * зовём [`lx_napi_run`]; больше NAPI ничего не требует.
 */
#define LX_NAPI_MAX 4
static struct napi_struct *lx_napi_pending[LX_NAPI_MAX];

/* Веха 199.6 — счётчики ПУЛЬСА: по ним видно, на каком шаге рвётся приём. Заводятся не от
 * любви к статистике: на живой машине владельца карта поднялась, кадр ушёл, а в ответ не пришло
 * ничего — и отличить «прерывание не дошло» от «карта молчит» было нечем, кроме догадок.
 * Считает их и планировщик (`lx_kit.c`), поэтому они не статические. */
unsigned long lx_isr_calls;    /* сколько раз звали обработчик прерывания */
unsigned long lx_isr_handled;  /* сколько раз он сказал «это моё» */
unsigned long lx_napi_polls;   /* сколько раз крутился опрос NAPI */
int lx_netdev_active(void);    /* определён ниже, в половине со syscall'ами */

/* Веха 199.7 — слово САМОЙ КАРТЫ в пульсе. Счётчики выше считают наш путь, а он может быть
 * исправен весь: карта принимает с провода и молча выбрасывает по своим правилам (так и вышло —
 * предел длины кадра стоял в 22 байта). Знает об этом только она, через свои регистры, а лезть
 * в них из общего шима нельзя: у каждой карты они свои. Поэтому ставит обработчик тот, кто
 * карту и поднимает, — драйвер-обёртка. Не поставил — пульс печатает как раньше. */
static void (*lx_net_diag_fn)(void);
void lx_net_set_diag(void (*fn)(void)) { lx_net_diag_fn = fn; }

void netif_napi_add(struct net_device *dev, struct napi_struct *napi, int (*poll)(struct napi_struct *, int))
{
	napi->dev = dev;
	napi->poll = poll;
	napi->weight = NAPI_POLL_WEIGHT;
	napi->state = 0;
	/* Тот же случай, что у списков адресов (Веха 133.4): обнулённая голова — битая, а не
	 * пустая. Сегодня этот список никто не обходит, но заводить его наполовину незачем. */
	INIT_LIST_HEAD(&napi->poll_list);
}
void netif_napi_set_irq(struct napi_struct *napi, int irq) { (void)napi; (void)irq; }
void netif_queue_set_napi(struct net_device *dev, unsigned int q, int type, struct napi_struct *napi)
{ (void)dev; (void)q; (void)type; (void)napi; }
void napi_enable(struct napi_struct *napi)
{ napi->state = 1; printk("lx_net: napi_enable\n"); }
void napi_disable(struct napi_struct *napi)
{
	napi->state = 0;
	for (int i = 0; i < LX_NAPI_MAX; i++)
		if (lx_napi_pending[i] == napi)
			lx_napi_pending[i] = NULL;
}

/* Поставить опрос в очередь. Дубликат не добавляем: `poll` не обязан быть повторно входимым,
 * и в Linux это гарантирует тот же самый бит «уже запланирован». */
void __napi_schedule(struct napi_struct *napi)
{
	for (int i = 0; i < LX_NAPI_MAX; i++)
		if (lx_napi_pending[i] == napi)
			return;
	for (int i = 0; i < LX_NAPI_MAX; i++)
		if (!lx_napi_pending[i]) {
			lx_napi_pending[i] = napi;
			return;
		}
	printk("lx_net: очередь NAPI полна — опрос потерян\n");
}

/* Разрешено ли планировать. Выключенный NAPI (до `ndo_open`, после `ndo_stop`) отвечает «нет»,
 * и это не формальность: до открытия кольца ещё не построены, а `poll` полез бы в них. */
bool napi_schedule_prep(struct napi_struct *napi)
{
	return napi->state == 1;
}

bool napi_complete_done(struct napi_struct *napi, int work_done)
{
	(void)work_done;
	for (int i = 0; i < LX_NAPI_MAX; i++)
		if (lx_napi_pending[i] == napi)
			lx_napi_pending[i] = NULL;
	return true;
}

/* Прогнать запланированные опросы — зовётся из холостого пути планировщика (softirq-контекст).
 * Возвращает, сколько работы сделано: ноль значит «можно спать». */
int lx_napi_run(void)
{
	int done = 0;

	for (int i = 0; i < LX_NAPI_MAX; i++) {
		struct napi_struct *n = lx_napi_pending[i];

		if (!n || !n->poll)
			continue;
		lx_napi_polls++;
		/* Драйвер сам снимет себя через napi_complete_done, когда кольцо опустеет. Если он
		 * этого не сделал (кадров больше, чем вес), опрос останется в очереди — и следующий
		 * заход продолжит с того же места, как и положено NAPI. */
		done += n->poll(n, n->weight);
	}
	return done;
}

/* ─── Приём: кадр УХОДИТ В СТЕК (Веха 195) ───────────────────────────────────
 *
 * Здесь была честная надпись «стека над драйвером ещё нет, кадр посчитан и отпущен». Стек есть
 * (`net-srv` на smoltcp) — не хватало дороги к нему. Теперь кадр отдаётся ядру
 * (`SYS_NETDEV`, op 1), а ядро кладёт его в ту же очередь, из которой `net-srv` читает кадры
 * обычной карты. Стек не меняется ни на строку и не знает, что карта сменила сторону кольца.
 */
#ifdef LX_HAVE_SYSCALL
static uintptr_t lx_netdev_cap = VOID_NO_CAP;
static struct net_device *lx_netdev_dev;
/* Веха 222.2 — ПРИНЯЛИ ЛИ НАС картой системы. Право быть картой есть у обоих драйверов машины
 * (проводного и беспроводного), а карта в системе одна: кто первый представился, тот ею и
 * работает, второму ядро отказывает.
 *
 * Раньше отказ только печатался, а кадры шли дальше — каждый вызовом в ядро, с копией до двух
 * килобайт, и каждый отвергался. На живом проводе это сотни тысяч пустых вызовов в секунду:
 * система вязла, и первой умирала USB-клавиатура — её ядро опрашивает, а опрос не успевал. */
static int lx_netdev_ours;
static unsigned long lx_rx_frames, lx_rx_lost, lx_tx_frames;
/* Веха 199.12 — ВЗЯТО из очереди ядра и СКОЛЬКО РАЗ карта отказалась взять кадр.
 *
 * «Отправлено» одно не отвечает на вопрос, где рвётся передача: ноль там значит и «кадра нам не
 * давали», и «давали, но карта его не взяла», а искать эти две вещи надо в разных местах —
 * первую в стеке и очереди ядра, вторую в драйвере. */
static unsigned long lx_tx_taken, lx_tx_busy;

void lx_net_set_netdev_cap(uintptr_t cap) { lx_netdev_cap = cap; }

/* Поднять интерфейс — то, что в Linux делает `dev_open`, а у нас не делал НИКТО.
 *
 * Обёртки звали `ndo_open` напрямую и считали, что этого довольно: кольца построены, движки
 * запущены, карта отвечает. А приёма всё равно не было, и причина оказалась в одной строке:
 * первое, на что смотрит обработчик прерывания `8139too`, — `netif_running(dev)`, то есть флаг
 * `IFF_UP`. Ставит его в Linux `dev_open` ПОСЛЕ успешного `ndo_open`; без него драйвер честно
 * решает, что интерфейс выключен, гасит маску прерываний и уходит. Снаружи это выглядело как
 * «карта поднята, прерывания сыплются, кадров нет».
 *
 * Здесь же и порядок: сперва открытие, и только по успеху — флаг. Наоборот значило бы объявить
 * поднятым то, что не поднялось.
 */
int lx_netdev_open(struct net_device *dev)
{
	const struct net_device_ops *ops = dev ? dev->netdev_ops : NULL;
	int err;

	if (!ops || !ops->ndo_open)
		return -EOPNOTSUPP;
	err = ops->ndo_open(dev);
	if (err)
		return err;
	dev->flags |= IFF_UP;
	/* `dev_set_rx_mode` — вторая половина `dev_open`: без неё карта не знает, какие адреса
	 * принимать. Драйверы обычно зовут её сами из `ndo_open`, но полагаться на это нельзя. */
	if (ops->ndo_set_rx_mode)
		ops->ndo_set_rx_mode(dev);
	return 0;
}

/* Объявить себя картой системы. Зовётся драйвером-обёрткой ПОСЛЕ `ndo_open`: раньше карта ещё
 * не принимает, и стек начал бы слать в пустоту. */
int lx_netdev_attach(struct net_device *dev)
{
	if (lx_netdev_cap == VOID_NO_CAP) {
		printk("lx_net: нет права быть картой — кадры в стек не пойдут\n");
		return 0;
	}
	lx_netdev_dev = dev;
	if (!vsys_netdev_attach(lx_netdev_cap, dev->dev_addr)) {
		/* Картой работает другой драйвер. Наше дело — замолчать: кадры этой карты стеку не
		 * нужны, и носить их туда значит жечь процессор за двоих. */
		lx_netdev_ours = 0;
		printk("lx_net: картой системы работает другой драйвер — наши кадры в стек не идут\n");
		return 0;
	}
	lx_netdev_ours = 1;
	printk("lx_net: мы — сетевая карта системы, кадры идут в стек\n");
	return 1;
}

static void lx_netdev_rx(struct sk_buff *skb)
{
	if (lx_netdev_cap == VOID_NO_CAP || !lx_netdev_ours || !skb->len)
		return;
	if (vsys_netdev_rx(lx_netdev_cap, skb->data, skb->len))
		lx_rx_frames++;
	else
		lx_rx_lost++;
	/* Каждый 1024-й кадр — строкой в журнал. Молчать нельзя (не узнать, идёт ли приём вовсе),
	 * печатать каждый — утопить журнал на первой же закачке: тысяча кадров это меньше секунды
	 * на сотне мегабит. */
	if ((lx_rx_frames & 1023) == 1)
		printk("lx_net: принято кадров %lu (потеряно %lu)\n", lx_rx_frames, lx_rx_lost);
}

/* Насос передачи: забрать у ядра кадры, которые стек просил отправить, и отдать их драйверу
 * его же `ndo_start_xmit`. Зовётся из холостого пути планировщика — там же, где NAPI.
 *
 * Почему опрос, а не «ядро зовёт нас»: позвать процесс ядро не может, оно может только его
 * РАЗБУДИТЬ — и будит (`wake_netdev_owner`). Проснувшись, планировщик заходит сюда, находит
 * кадр и отправляет его. Вхолостую этот заход не делается: спящий процесс не крутится.
 */
/* Веха 199.6 — ПУЛЬС: строка о том, где рвётся приём.
 *
 *   ISR 0            — обработчик не зовут вовсе: прерывание не заведено или шим спит;
 *   ISR>0, «моё» 0   — обработчик зовут, но карта говорит «это не я»: не доходит прерывание,
 *                      и опрос регистра причин ничего не находит;
 *   NAPI 0 при «моё»>0 — карта сказала «моё», а опрос не запланирован (ошибка в шиме);
 *   NAPI>0, принято 0 — опрос идёт, а кольцо приёма пусто: карта не пишет кадры (DMA, фильтр).
 *
 * ── Веха 199.8: «принято > 0» не значит «всё хорошо НАВСЕГДА» ───────────────────────────────
 *
 * Условие замолкания было `lx_rx_frames > 0` — то есть первый же принятый кадр выключал
 * диагностику до перезагрузки. На машине владельца приём отработал ровно первую секунду (DHCP
 * взял адрес, шлюз ответил на ICMP) и встал — а сказать об этом было уже некому: пульс замолк
 * на том самом кадре, который доказал, что приём был. Диагностика, которая видит только
 * «никогда не начиналось» и слепа к «работало и перестало», — половина диагностики.
 *
 * Теперь пульс говорит РОВНО В ОДНОМ случае: «мы говорим, а нам не отвечают» — с прошлого раза
 * кадры уходили, а не пришло ни одного. В тишине (никто ничего не шлёт) он молчит, при живом
 * приёме молчит тоже: журнал засоряет только настоящая поломка.
 *
 * Отдельно — ОДИН рассказ карты сразу после подъёма, безусловный. Он стоит трёх строк на
 * загрузку и уже окупился: именно в нём был виден предел кадра в 22 байта (Веха 199.7).
 */
void lx_net_pulse(unsigned long now_jiffies)
{
	static unsigned long next, slice, seen_rx, seen_tx;
	static int said_once;
	int talking, deaf;

	if (!lx_netdev_active())
		return;
	if (next && (long)(now_jiffies - next) < 0)
		return;
	/* Веха 199.9 — снова ДВЕ секунды. Пять я поставил, чтобы три строки пульса влезали в одну
	 * фотографию, — и это вышло боком: владелец пингует, получает «нет ответа» и сразу смотрит
	 * `klog`, а пульс в это окно не попадает. Теперь он и так молчит, пока сеть жива, поэтому
	 * частить ему нечем; важнее успеть сказать между командой и взглядом в журнал. */
	next = now_jiffies + 2 * HZ;

	talking = lx_tx_frames != seen_tx;  /* с прошлого раза мы что-то отправляли */
	deaf    = lx_rx_frames == seen_rx;  /* и не приняли ничего */
	seen_tx = lx_tx_frames;
	seen_rx = lx_rx_frames;

	/* Веха 199.9 — РЕДКИЙ СРЕЗ, даже когда всё выглядит хорошо.
	 *
	 * Условия «отправляли и не приняли» мало: в живой домашней сети широковещание идёт само
	 * собой (ARP соседей), приём растёт — и пульс промолчит, хотя `ping` не отвечает. Раз в
	 * полминуты говорим безусловно: три строки за тридцать секунд журнал переживёт, а
	 * отладка на машине, куда можно смотреть только через фотографию экрана, без них встаёт.
	 * Когда сеть на железе перестанет быть расследованием, этот срез уйдёт. */
	if (said_once && !(talking && deaf) && (long)(now_jiffies - slice) < 0)
		return;
	slice = now_jiffies + 30 * HZ;
	said_once = 1;

	printk("lx_net: пульс — ISR %lu (моё %lu), NAPI %lu, принято %lu, взято %lu,"
	       " отправлено %lu (отказов %lu)\n",
	       lx_isr_calls, lx_isr_handled, lx_napi_polls, lx_rx_frames,
	       lx_tx_taken, lx_tx_frames, lx_tx_busy);
	/* Дальше своё слово говорит сама карта — то, чего шим знать не может (её регистры и её
	 * собственные счётчики). Кто это печатает, решает драйвер-обёртка. */
	if (lx_net_diag_fn)
		lx_net_diag_fn();
}

/// Веха 199.18 — сколько кадров дошло до стека. Нужно СТОРОЖУ ПРИЁМА: только по этому числу
/// видно, что карта принимает с провода, а в систему не попадает ничего.
unsigned long lx_net_rx_count(void) { return lx_rx_frames; }

/// Веха 199.5 — работаем ли мы сейчас картой системы.
int lx_netdev_active(void)
{
	return lx_netdev_cap != VOID_NO_CAP && lx_netdev_dev != 0;
}

/* Веха 199.12 — ОТЛОЖЕННЫЙ КАДР. `NETDEV_TX_BUSY` в Linux означает «повтори позже», и повторять
 * обязан тот, кто отдаёт. Раньше отказ драйвера здесь терялся дважды: кадр молча пропадал, а его
 * буфер не возвращался в арену вовсе. Держим один кадр и пробуем его на следующем обороте — так
 * же, как это делает очередь qdisc.
 *
 * Один, а не очередь: кадры ждут своего часа в очереди ЯДРА, и заводить вторую тут значило бы
 * держать их в двух местах сразу. Пока отложенный не ушёл, из ядра мы не берём ничего. */
static unsigned char lx_tx_hold[1600];
static size_t lx_tx_hold_len;

/* Отдать кадр драйверу. 1 — взял, 0 — занят (надо повторить позже). */
static int lx_xmit_one(const unsigned char *frame, size_t n)
{
	const struct net_device_ops *ops = lx_netdev_dev->netdev_ops;
	struct sk_buff *skb;

	if (!ops || !ops->ndo_start_xmit)
		return 0;
	/* Буфер кадра берётся из АРЕНЫ DMA (`lx_skb_alloc`): карта будет читать его сама, и адрес
	 * ей нужен физический. Кадр со стека сюда копируется — второй раз за путь, и это цена
	 * того, что очереди живут в ядре (см. `kernel/src/net.rs`). */
	skb = __netdev_alloc_skb(lx_netdev_dev, n, 0);
	if (!skb) {
		printk("lx_net: нет буфера под исходящий кадр — откладываю\n");
		return 0;
	}
	memcpy(skb_put(skb, n), frame, n);
	skb->dev = lx_netdev_dev;
	if (ops->ndo_start_xmit(skb, lx_netdev_dev) == NETDEV_TX_OK) {
		lx_tx_frames++;
		if ((lx_tx_frames & 1023) == 1)
			printk("lx_net: отправлено кадров %lu\n", lx_tx_frames);
		return 1;
	}
	/* Драйвер не взял. Раньше об этом не знал никто, и кадр исчезал бесследно. */
	lx_tx_busy++;
	if (lx_tx_busy == 1 || (lx_tx_busy % 64) == 0)
		printk("lx_net: карта не взяла кадр (занята) — отказов %lu\n", lx_tx_busy);
	dev_kfree_skb(skb);
	return 0;
}

int lx_netdev_pump(void)
{
	unsigned char buf[1600];
	int sent = 0;

	if (lx_netdev_cap == VOID_NO_CAP || !lx_netdev_ours || !lx_netdev_dev)
		return 0;
	/* Сперва — отложенный: порядок кадров важнее скорости. */
	if (lx_tx_hold_len) {
		if (!lx_xmit_one(lx_tx_hold, lx_tx_hold_len))
			return 0;
		lx_tx_hold_len = 0;
		sent++;
	}
	for (;;) {
		size_t n = vsys_netdev_tx_pop(lx_netdev_cap, buf, sizeof(buf));

		if (!n)
			break;
		lx_tx_taken++; /* ВЗЯЛИ из очереди; отдали ли карте — вопрос отдельный */
		if (!lx_xmit_one(buf, n)) {
			memcpy(lx_tx_hold, buf, n);
			lx_tx_hold_len = n;
			break;
		}
		sent++;
	}
	return sent;
}
#else
/* Сборка-«вычислялка» (без syscall'ов): карты нет, отдавать кадры некому. */
static void lx_netdev_rx(struct sk_buff *skb) { (void)skb; }
int lx_netdev_pump(void) { return 0; }
int lx_netdev_active(void) { return 0; }
void lx_net_pulse(unsigned long now_jiffies) { (void)now_jiffies; }
#endif

void napi_gro_receive(struct napi_struct *napi, struct sk_buff *skb)
{
	(void)napi;
	lx_netdev_rx(skb);
	dev_kfree_skb(skb);
}
/* Веха 193 — приём мимо GRO. Разница с предыдущим у нас нулевая: склеивать сегменты некому,
 * и оба пути кончаются одним — кадр уходит в стек и отпускается. */
void netif_receive_skb(struct sk_buff *skb)
{
	lx_netdev_rx(skb);
	dev_kfree_skb(skb);
}
struct sk_buff *napi_get_frags(struct napi_struct *napi) { (void)napi; return NULL; }
void napi_free_frags(struct napi_struct *napi) { (void)napi; }
int  napi_gro_frags(struct napi_struct *napi) { (void)napi; return 0; }

/* ─ MII: опрос линка (Веха 193) ─
 *
 * `mii_check_media` в Linux читает BMSR, сравнивает с прошлым состоянием и объявляет
 * `netif_carrier_on/off`. Бит BMSR_LSTATUS ЗАЛИПАЮЩИЙ: он помнит, что линк пропадал, поэтому
 * читать его надо дважды — первое чтение сбрасывает память, второе говорит правду. Урок этот
 * уже оплачен на atl1c (Веха 133), и повторять его здесь не будем.
 */
int mii_link_ok(struct mii_if_info *mii)
{
	int bmsr;

	if (!mii || !mii->mdio_read)
		return 0;
	mii->mdio_read(mii->dev, mii->phy_id, MII_BMSR);       /* сбросить залипание */
	bmsr = mii->mdio_read(mii->dev, mii->phy_id, MII_BMSR);
	return (bmsr & BMSR_LSTATUS) ? 1 : 0;
}

/* CRC32 адреса для фильтра групповых кадров. `ether_crc_le` у нас уже есть (Веха 68); это её
 * старший-битом-вперёд близнец, которым пользуются старые карты — RTL8139 в их числе. */
u32 ether_crc(int length, unsigned char *data)
{
	u32 crc = 0xffffffff;
	int i;

	while (--length >= 0) {
		u8 c = *data++;

		for (i = 0; i < 8; i++, c >>= 1)
			crc = (crc << 1) ^ ((((crc >> 31) ^ c) & 1) ? 0x04c11db7 : 0);
	}
	return crc;
}

/* MII-ioctl из userspace. Слать их у нас некому — `ifconfig` в VOID не существует, — но
 * обработчик драйвера объявлен, и звать его должно быть чем. Отвечаем честным отказом, а не
 * правдоподобным нулём: молчаливый успех здесь означал бы «PHY настроен», когда он не тронут. */
int generic_mii_ioctl(struct mii_if_info *mii, struct mii_ioctl_data *mii_data, int cmd,
		      unsigned int *duplex_changed)
{
	(void)mii; (void)mii_data; (void)cmd;
	if (duplex_changed)
		*duplex_changed = 0;
	return -EOPNOTSUPP;
}

/* Перезапуск автосогласования: взвести BMCR_ANRESTART поверх BMCR_ANENABLE. */
int mii_nway_restart(struct mii_if_info *mii)
{
	int bmcr;

	if (!mii || !mii->mdio_read || !mii->mdio_write)
		return -EINVAL;
	bmcr = mii->mdio_read(mii->dev, mii->phy_id, MII_BMCR);
	if (!(bmcr & BMCR_ANENABLE))
		return -EINVAL;
	mii->mdio_write(mii->dev, mii->phy_id, MII_BMCR, bmcr | BMCR_ANRESTART);
	return 0;
}

/* Описание линка для ethtool. Читателя у этих чисел пока нет — утилиты `ethtool` в VOID не
 * существует, — но заполняем честно: молча оставить нули значило бы соврать первому же, кто
 * сюда посмотрит. */
int mii_ethtool_get_link_ksettings(struct mii_if_info *mii,
				   struct ethtool_link_ksettings *cmd)
{
	int bmcr, lpa;

	if (!mii || !mii->mdio_read)
		return -EINVAL;
	bmcr = mii->mdio_read(mii->dev, mii->phy_id, MII_BMCR);
	lpa  = mii->mdio_read(mii->dev, mii->phy_id, MII_LPA);
	cmd->base.phy_address = mii->phy_id;
	cmd->base.autoneg = (bmcr & BMCR_ANENABLE) ? 1 : 0;
	cmd->base.duplex = mii->full_duplex ? 1 : 0;
	cmd->base.speed = (lpa & (LPA_100FULL | LPA_100HALF)) ? 100 : 10;
	cmd->link_modes.lp_advertising = (u32)lpa;
	cmd->link_modes.advertising = (u32)mii->advertising;
	return 0;
}

int mii_ethtool_set_link_ksettings(struct mii_if_info *mii,
				   const struct ethtool_link_ksettings *cmd)
{
	/* Задавать скорость руками мы не умеем и делать вид не будем: у драйвера и так есть
	 * автосогласование, а неполная реализация здесь молча расходилась бы с тем, что на
	 * проводе. Отказ увидит тот, кто попросил, — и это правда. */
	(void)mii; (void)cmd;
	return -EOPNOTSUPP;
}

unsigned int mii_check_media(struct mii_if_info *mii, unsigned int ok_to_print,
			     unsigned int init_media)
{
	int ok = mii_link_ok(mii);

	(void)init_media;
	if (ok)
		netif_carrier_on(mii->dev);
	else
		netif_carrier_off(mii->dev);
	if (ok_to_print)
		printk("[mii] линк %s\n", ok ? "есть" : "пропал");
	return ok ? 1 : 0;
}

/* ─ sk_buff: аллокация/линейка (реальные — на них встанет TX/RX) ─ */
static struct sk_buff *lx_skb_alloc(unsigned int len)
{
	struct sk_buff *skb = kmalloc(sizeof(*skb), 0);
	unsigned int room = len + NET_SKB_PAD;
	if (!skb) return NULL;
	memset(skb, 0, sizeof(*skb));
	/* Данные — ИЗ АРЕНЫ DMA: в них будет писать сама карта (Веха 133). Арены нет (сборка без
	 * syscall'ов) — берём кучу: там пакетов не бывает, считается только логика. */
	/* Каждый 128-й буфер — в лог. Кольцо приёма это 512 буферов; печатать все значит утопить
	 * журнал, а не печатать ничего — не узнать, дошли ли мы до их раздачи и где встали. */
	{
		static unsigned n;
		if ((n++ & 127) == 0)
			printk("lx_net: буфер приёма №%u (%u байт)\n", n - 1, room);
	}
	skb->head = lx_arena_alloc(room);
	if (!skb->head) {
		skb->head = kmalloc(room, 0);
		skb->lx_heap = 1;
	}
	if (!skb->head) { kfree(skb); return NULL; }
	skb->data = skb->head + NET_SKB_PAD;
	skb->tail = skb->data;
	skb->end  = skb->head + room;
	skb->truesize = room;
	return skb;
}
struct sk_buff *__netdev_alloc_skb(struct net_device *dev, unsigned int len, gfp_t gfp)
{ (void)dev; (void)gfp; return lx_skb_alloc(len); }
struct sk_buff *napi_alloc_skb(struct napi_struct *napi, unsigned int len) { (void)napi; return lx_skb_alloc(len); }
struct sk_buff *build_skb(void *data, unsigned int frag_size) { (void)data; return lx_skb_alloc(frag_size); }
struct sk_buff *napi_build_skb(void *data, unsigned int frag_size) { (void)data; return lx_skb_alloc(frag_size); }
void dev_kfree_skb(struct sk_buff *skb)
{
	if (!skb) return;
	if (skb->lx_heap)
		kfree(skb->head);          /* сборка без арены — память из кучи */
	else if (skb->head)
		lx_arena_free_block(skb->head, skb->truesize); /* Веха 134: арена принимает назад */
	kfree(skb);
}
void dev_kfree_skb_any(struct sk_buff *skb) { dev_kfree_skb(skb); }
void consume_skb(struct sk_buff *skb) { dev_kfree_skb(skb); }
void napi_consume_skb(struct sk_buff *skb, int budget) { (void)budget; dev_kfree_skb(skb); }
void *netdev_alloc_frag(unsigned int fragsz) { return kmalloc(fragsz, 0); }
void skb_free_frag(void *data) { kfree(data); }

void *skb_put(struct sk_buff *skb, unsigned int len)
{ void *tail = skb->tail; skb->tail += len; skb->len += len; return tail; }
void *skb_put_data(struct sk_buff *skb, const void *data, unsigned int len)
{ void *tail = skb_put(skb, len); memcpy(tail, data, len); return tail; }
void skb_reserve(struct sk_buff *skb, int len) { skb->data += len; skb->tail += len; }
void skb_copy_to_linear_data(struct sk_buff *skb, const void *from, unsigned int len)
{ memcpy(skb->data, from, len); }
/* Досчёта контрольной суммы у нас НЕТ, и это сказано вслух. В Linux эта функция копирует кадр
 * и попутно считает сумму для карт, которые не умеют считать её сами. RTL8139 умеет; появится
 * карта, которая не умеет, — сумму придётся считать здесь, и молчаливое копирование станет
 * ошибкой, которую будет искать не в этом файле. */
void skb_copy_and_csum_dev(const struct sk_buff *skb, u8 *to)
{ memcpy(to, skb->data, skb->len); }
void skb_trim(struct sk_buff *skb, unsigned int len)
{ if (len < skb->len) { skb->len = len; skb->tail = skb->data + len; } }
int  skb_cow_head(struct sk_buff *skb, unsigned int headroom) { (void)skb; (void)headroom; return 0; }
int  skb_pad(struct sk_buff *skb, int pad) { (void)skb; (void)pad; return 0; }
int  pskb_trim(struct sk_buff *skb, unsigned int len) { skb_trim(skb, len); return 0; }
void *__pskb_pull_tail(struct sk_buff *skb, int delta) { (void)delta; return skb->data; }
void skb_fill_page_desc(struct sk_buff *skb, int i, struct page *page, int off, int size)
{ skb_frag_t *f = &skb_shinfo(skb)->frags[i]; f->page = page; f->offset = off; f->size = size; skb_shinfo(skb)->nr_frags = i + 1; }
dma_addr_t skb_frag_dma_map(struct device *dev, const skb_frag_t *frag, size_t offset, size_t size, int dir)
{ (void)dev; (void)size; (void)dir; return (dma_addr_t)(unsigned long)page_address(frag->page) + frag->offset + offset; }
void skb_tx_timestamp(struct sk_buff *skb) { (void)skb; }
void __vlan_hwaccel_put_tag(struct sk_buff *skb, __be16 proto, u16 tci) { (void)skb; (void)proto; (void)tci; }
void tcp_v6_gso_csum_prep(struct sk_buff *skb) { (void)skb; }

/* ─ IRQ: регистрируем обработчик в планировщике (Веха 72). Он спит на IRQ-cap (vsys_irq_wait) в
 *   idle-пути и по прерыванию карты зовёт handler. Без IRQ-cap («вычислялка» Веха 68 / нет права) —
 *   no-op, как раньше. irqreturn_t (enum, int-размер) → lx_irq_handler_t (int) кастуем. ─ */
int  request_irq(unsigned int irq, irq_handler_t h, unsigned long flags, const char *name, void *dev)
{
	(void)flags;
	printk("lx_net: request_irq('%s', линия %u)\n", name ? name : "?", irq);
#ifdef LX_HAVE_SYSCALL
	/* Веха 199.6 — обработчик заводим ДАЖЕ БЕЗ ПРАВА НА ПРЕРЫВАНИЕ.
	 *
	 * Раньше здесь стояло условие: нет права — забыть обработчик и вернуть успех. Драйвер после
	 * этого считал себя настроенным, а принять не мог НИЧЕГО: звать его было некому. И ни одной
	 * строки об этом — то есть самый тихий вид поломки из возможных.
	 *
	 * Теперь планировщик в таком случае просто ОПРАШИВАЕТ карту (см. `lx_sched_run`): медленнее,
	 * зато работает. Право на прерывание становится оптимизацией, а не условием жизни. */
	lx_irq_register((int)irq, lx_irq_cap, (lx_irq_handler_t)h, dev);
	if (lx_irq_cap == VOID_NO_CAP)
		printk("lx_net: права на прерывание нет — карту буду опрашивать\n");
	return 0;
#endif
	(void)irq; (void)h; (void)dev;
	return 0;
}
void free_irq(unsigned int irq, void *dev)
{
	(void)dev;
#ifdef LX_HAVE_SYSCALL
	lx_irq_unregister((int)irq);
#endif
	(void)irq;
}
void disable_irq(unsigned int irq) { (void)irq; }
void enable_irq(unsigned int irq) { (void)irq; }
void synchronize_irq(unsigned int irq) { (void)irq; }

/* ─ порт-IO x86 (workaround 82547) ─ */
void outl(u32 value, unsigned long port) { (void)value; (void)port; }
u32  inl(unsigned long port) { (void)port; return 0; }
void outb(u8 value, unsigned long port) { (void)value; (void)port; }
u8   inb(unsigned long port) { (void)port; return 0; }

/* ─ PCI-довески поверх Вехи 67 ─ */
int  pci_wake_from_d3(struct pci_dev *dev, bool enable) { (void)dev; (void)enable; return 0; }
int  pcix_get_mmrbc(struct pci_dev *dev) { (void)dev; return 2048; }
int  pcix_set_mmrbc(struct pci_dev *dev, int mmrbc) { (void)dev; (void)mmrbc; return 0; }
int  device_set_wakeup_enable(struct device *dev, bool enable) { (void)dev; (void)enable; return 0; }
int  device_wakeup_enable(struct device *dev) { (void)dev; return 0; }

/* ─ ethtool: набор операций поднимем с e1000_ethtool.c (пока — no-op) ─ */
void e1000_set_ethtool_ops(struct net_device *netdev) { (void)netdev; }
