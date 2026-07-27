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
#include <linux/interrupt.h>
#include <linux/kernel.h>
#include <linux/mm.h>
#include <linux/netdevice.h>
#include <linux/pci.h>
#include <linux/skbuff.h>
#include <linux/slab.h>
#include <linux/string.h>

/* Настоящий DMA поверх DMA-cap VOID (SYS_DMA_ALLOC) — только в сборке драйвера (-DLX_HAVE_SYSCALL,
 * -I${void-libc}/lib). В сборке-«вычислялке» (Веха 68) DMA не зовётся → остаётся куча-версия. */
#ifdef LX_HAVE_SYSCALL
#include <syscall.h> /* vsys_dma_alloc / VOID_NO_CAP */
static uintptr_t lx_dma_cap    = VOID_NO_CAP;
static uintptr_t lx_dma_va_next = 0x58000000UL; /* DMA_BASE, как в lx_emul (Веха 54) */
void lx_net_set_dma_cap(uintptr_t cap) { lx_dma_cap = cap; }
#endif

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
#ifdef LX_HAVE_SYSCALL
	/* Реальный DMA: страница по DMA-cap, её ФИЗ-адрес — device-доступный (одна страница ≤ 4 КиБ). */
	if (lx_dma_cap != VOID_NO_CAP && size <= 4096) {
		uintptr_t va = lx_dma_va_next;
		uintptr_t pa = vsys_dma_alloc(lx_dma_cap, va);
		if (pa == VOID_NO_CAP) { *handle = 0; return NULL; }
		lx_dma_va_next += 4096;
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
dma_addr_t dma_map_single(struct device *dev, void *ptr, size_t size, int dir)
{ (void)dev; (void)size; (void)dir; return (dma_addr_t)(unsigned long)ptr; }
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
struct net_device *alloc_etherdev(int sizeof_priv)
{
	struct net_device *dev = kmalloc(sizeof(*dev), 0);
	if (!dev) return NULL;
	memset(dev, 0, sizeof(*dev));
	dev->lx_priv = kmalloc(sizeof_priv, 0);
	if (dev->lx_priv) memset(dev->lx_priv, 0, sizeof_priv);
	dev->mc.count = 0; dev->uc.count = 0;
	return dev;
}
void free_netdev(struct net_device *dev)
{ if (dev) { kfree(dev->lx_priv); kfree(dev); } }
int register_netdev(struct net_device *dev)
{ printk("lx_net: register_netdev('%s')\n", dev->name[0] ? dev->name : "ethN"); return 0; }
void unregister_netdev(struct net_device *dev) { (void)dev; }

__be16 eth_type_trans(struct sk_buff *skb, struct net_device *dev)
{ (void)dev; return skb ? skb->protocol : 0; }
int eth_validate_addr(struct net_device *dev)
{ return is_valid_ether_addr(dev->dev_addr) ? 0 : -22 /* -EINVAL */; }
void eth_hw_addr_random(struct net_device *dev) { memset(dev->dev_addr, 0x02, ETH_ALEN); }

struct netdev_queue *netdev_get_tx_queue(struct net_device *dev, unsigned int index)
{ (void)dev; (void)index; return NULL; }

/* ─ netif_* очереди/несущая (оживут при open/link на след. вехе) ─ */
void netif_start_queue(struct net_device *dev) { (void)dev; }
void netif_stop_queue(struct net_device *dev) { (void)dev; }
void netif_wake_queue(struct net_device *dev) { (void)dev; }
void netif_tx_disable(struct net_device *dev) { (void)dev; }
bool netif_queue_stopped(const struct net_device *dev) { (void)dev; return false; }
bool netif_running(const struct net_device *dev) { return dev->flags & IFF_UP; }
void netif_carrier_on(struct net_device *dev) { (void)dev; }
void netif_carrier_off(struct net_device *dev) { (void)dev; }
bool netif_carrier_ok(const struct net_device *dev) { (void)dev; return true; }
void netif_device_attach(struct net_device *dev) { (void)dev; }
void netif_device_detach(struct net_device *dev) { (void)dev; }

/* ─ NAPI (оживёт при RX-поллинге на след. вехе) ─ */
void netif_napi_add(struct net_device *dev, struct napi_struct *napi, int (*poll)(struct napi_struct *, int))
{ napi->dev = dev; napi->poll = poll; napi->weight = NAPI_POLL_WEIGHT; napi->state = 0; }
void netif_napi_set_irq(struct napi_struct *napi, int irq) { (void)napi; (void)irq; }
void netif_queue_set_napi(struct net_device *dev, unsigned int q, int type, struct napi_struct *napi)
{ (void)dev; (void)q; (void)type; (void)napi; }
void napi_enable(struct napi_struct *napi) { napi->state = 1; }
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
	skb->head = kmalloc(room, 0);
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
void dev_kfree_skb(struct sk_buff *skb) { if (skb) { kfree(skb->head); kfree(skb); } }
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

/* ─ IRQ (реальный IRQ — задача на SYS_IRQ_WAIT, следующая веха) ─ */
int  request_irq(unsigned int irq, irq_handler_t h, unsigned long flags, const char *name, void *dev)
{ (void)irq; (void)h; (void)flags; (void)name; (void)dev; return 0; }
void free_irq(unsigned int irq, void *dev) { (void)irq; (void)dev; }
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
