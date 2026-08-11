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
		uintptr_t va = lx_dma_va_next;
		uintptr_t pa = vsys_dma_alloc_n(lx_dma_cap, va, pages);
		if (pa == VOID_NO_CAP) { *handle = 0; return NULL; }
		lx_dma_va_next += pages * 4096;
		memset((void *)va, 0, size);
		*handle = (dma_addr_t)pa;
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
	(void)dev; (void)size; (void)handle;
#ifdef LX_HAVE_SYSCALL
	if (lx_dma_cap != VOID_NO_CAP) return; /* DMA-страницы не возвращаем (одноразовый bring-up) */
#endif
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
{ (void)dev; printk("lx_net: netif_carrier_on — несущая есть\n"); }
void netif_carrier_off(struct net_device *dev)
{ (void)dev; printk("lx_net: netif_carrier_off\n"); }
bool netif_carrier_ok(const struct net_device *dev) { (void)dev; return true; }
void netif_device_attach(struct net_device *dev) { (void)dev; }
void netif_device_detach(struct net_device *dev) { (void)dev; }

/* ─ NAPI (оживёт при RX-поллинге на след. вехе) ─ */
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
void napi_disable(struct napi_struct *napi) { napi->state = 0; }
void __napi_schedule(struct napi_struct *napi) { (void)napi; }
bool napi_schedule_prep(struct napi_struct *napi) { (void)napi; return false; }
bool napi_complete_done(struct napi_struct *napi, int work_done) { (void)napi; (void)work_done; return true; }
void napi_gro_receive(struct napi_struct *napi, struct sk_buff *skb) { (void)napi; dev_kfree_skb(skb); }
struct sk_buff *napi_get_frags(struct napi_struct *napi) { (void)napi; return NULL; }
void napi_free_frags(struct napi_struct *napi) { (void)napi; }
int  napi_gro_frags(struct napi_struct *napi) { (void)napi; return 0; }

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
	if (lx_irq_cap != VOID_NO_CAP) {
		lx_irq_register((int)irq, lx_irq_cap, (lx_irq_handler_t)h, dev);
		return 0;
	}
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
