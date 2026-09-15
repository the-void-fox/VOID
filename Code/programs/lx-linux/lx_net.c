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
static unsigned long lx_rx_frames, lx_rx_lost, lx_tx_frames;

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
		printk("lx_net: ядро не приняло нас картой\n");
		return 0;
	}
	printk("lx_net: мы — сетевая карта системы, кадры идут в стек\n");
	return 1;
}

static void lx_netdev_rx(struct sk_buff *skb)
{
	if (lx_netdev_cap == VOID_NO_CAP || !skb->len)
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
/// Веха 199.5 — работаем ли мы сейчас картой системы.
int lx_netdev_active(void)
{
	return lx_netdev_cap != VOID_NO_CAP && lx_netdev_dev != 0;
}

int lx_netdev_pump(void)
{
	unsigned char buf[1600];
	int sent = 0;

	if (lx_netdev_cap == VOID_NO_CAP || !lx_netdev_dev)
		return 0;
	for (;;) {
		size_t n = vsys_netdev_tx_pop(lx_netdev_cap, buf, sizeof(buf));
		struct sk_buff *skb;
		const struct net_device_ops *ops = lx_netdev_dev->netdev_ops;

		if (!n)
			break;
		if (!ops || !ops->ndo_start_xmit)
			break;
		/* Буфер кадра берётся из АРЕНЫ DMA (`lx_skb_alloc`): карта будет читать его сама, и
		 * адрес ей нужен физический. Кадр со стека сюда копируется — второй раз за путь, и
		 * это цена того, что очереди живут в ядре (см. `kernel/src/net.rs`). */
		skb = __netdev_alloc_skb(lx_netdev_dev, n, 0);
		if (!skb) {
			printk("lx_net: нет буфера под исходящий кадр — потерян\n");
			break;
		}
		memcpy(skb_put(skb, n), buf, n);
		skb->dev = lx_netdev_dev;
		if (ops->ndo_start_xmit(skb, lx_netdev_dev) == NETDEV_TX_OK)
			lx_tx_frames++;
		sent++;
		if ((lx_tx_frames & 1023) == 1)
			printk("lx_net: отправлено кадров %lu\n", lx_tx_frames);
	}
	return sent;
}
#else
/* Сборка-«вычислялка» (без syscall'ов): карты нет, отдавать кадры некому. */
static void lx_netdev_rx(struct sk_buff *skb) { (void)skb; }
int lx_netdev_pump(void) { return 0; }
int lx_netdev_active(void) { return 0; }
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
