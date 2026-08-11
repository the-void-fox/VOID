/* linux/skbuff.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * `sk_buff` — сетевой буфер ядра. Подмножество под e1000: линейные данные (`data`/`len`/`tail`),
 * фрагменты (скаттер-гэзер TX) и `skb_shared_info` (GSO/фрагменты). В отличие от ядра shinfo у нас
 * ВЛОЖЕН в буфер (мы сами его аллоцируем) — layout не бинарно-совместим, но семантика та же.
 * Функции объявлены, тела — в lx_kit.c (реальный формат кадра на стыке с net-srv, Веха 34).
 */
#ifndef _LINUX_SKBUFF_H_SHIM
#define _LINUX_SKBUFF_H_SHIM

#include <linux/types.h>
#include <linux/mm.h>
#include <linux/gfp.h>
#include <asm/byteorder.h>

#define MAX_SKB_FRAGS 16
#define NET_SKB_PAD   32
#define NET_IP_ALIGN  2
#define SMP_CACHE_BYTES 64
#define SKB_DATA_ALIGN(x) (((x) + (SMP_CACHE_BYTES - 1)) & ~(SMP_CACHE_BYTES - 1))

/* Типы GSO (сегментация). */
#define SKB_GSO_TCPV4 (1 << 0)
#define SKB_GSO_TCPV6 (1 << 4)

/* Контроль контрольной суммы (skb->ip_summed). */
#define CHECKSUM_NONE        0
#define CHECKSUM_UNNECESSARY 1
#define CHECKSUM_COMPLETE    2
#define CHECKSUM_PARTIAL     3

/* Фрагмент буфера (страница + смещение + размер). */
typedef struct skb_frag {
	struct page  *page;
	unsigned int  offset;
	unsigned int  size;
} skb_frag_t;

struct skb_shared_info {
	unsigned char nr_frags;
	unsigned short gso_size;
	unsigned short gso_segs;
	unsigned int   gso_type;
	skb_frag_t     frags[MAX_SKB_FRAGS];
};

struct net_device;
struct napi_struct;

struct sk_buff {
	struct sk_buff  *next;
	struct sk_buff  *prev;
	struct net_device *dev;
	unsigned int     len;       /* всего байт (линейка + фрагменты) */
	unsigned int     data_len;  /* байт во фрагментах */
	unsigned int     truesize;
	/* Веха 133 — данные взяты из КУЧИ, а не из арены DMA (сборки без syscall'ов). Различать
	 * обязательно: арена не возвращает память, куча возвращает. */
	unsigned char    lx_heap;
	__be16           protocol;
	__u16            csum_offset;
	__u16            csum_start;
	__u8             ip_summed;  /* CHECKSUM_* */
	__u8             no_fcs;     /* не добавлять FCS аппаратно */
	unsigned char   *head;       /* начало выделенного буфера */
	unsigned char   *data;       /* начало полезных данных */
	unsigned char   *tail;       /* конец полезных данных */
	unsigned char   *end;        /* конец буфера */
	unsigned int     transport_header; /* смещение L4 */
	unsigned int     network_header;   /* смещение L3 */
	unsigned int     mac_header;        /* смещение L2 */
	struct skb_shared_info shinfo;
};

#define skb_shinfo(skb) (&(skb)->shinfo)

/* Аллокация/освобождение (тела в lx_kit.c). */
struct sk_buff *__netdev_alloc_skb(struct net_device *dev, unsigned int len, gfp_t gfp);
static inline struct sk_buff *netdev_alloc_skb(struct net_device *dev, unsigned int len)
{ return __netdev_alloc_skb(dev, len, 0); }
struct sk_buff *napi_alloc_skb(struct napi_struct *napi, unsigned int len);
struct sk_buff *build_skb(void *data, unsigned int frag_size);
struct sk_buff *napi_build_skb(void *data, unsigned int frag_size);
void dev_kfree_skb(struct sk_buff *skb);
void dev_kfree_skb_any(struct sk_buff *skb);
void consume_skb(struct sk_buff *skb);
void napi_consume_skb(struct sk_buff *skb, int budget);
void *netdev_alloc_frag(unsigned int fragsz);
void skb_free_frag(void *data);

/* Манипуляции линейной областью (тела в lx_kit.c). */
void *skb_put(struct sk_buff *skb, unsigned int len);
void *skb_put_data(struct sk_buff *skb, const void *data, unsigned int len);
void  skb_reserve(struct sk_buff *skb, int len);
void  skb_trim(struct sk_buff *skb, unsigned int len);
int   skb_cow_head(struct sk_buff *skb, unsigned int headroom);
int   skb_pad(struct sk_buff *skb, int pad);
static inline int eth_skb_pad(struct sk_buff *skb) { return skb_pad(skb, 60 - (int)skb->len); }

static inline unsigned int skb_headlen(const struct sk_buff *skb)
{ return skb->len - skb->data_len; }
static inline unsigned char *skb_tail_pointer(const struct sk_buff *skb)
{ return skb->tail; }
static inline unsigned int skb_headroom(const struct sk_buff *skb)
{ return (unsigned int)(skb->data - skb->head); }
static inline unsigned int skb_tailroom(const struct sk_buff *skb)
{ return (unsigned int)(skb->end - skb->tail); }

/* Смещения заголовков (offload). */
static inline unsigned char *skb_transport_header(const struct sk_buff *skb)
{ return skb->head + skb->transport_header; }
static inline unsigned char *skb_network_header(const struct sk_buff *skb)
{ return skb->head + skb->network_header; }
static inline int skb_transport_offset(const struct sk_buff *skb)
{ return (int)(skb->transport_header - (unsigned int)(skb->data - skb->head)); }
static inline int skb_network_offset(const struct sk_buff *skb)
{ return (int)(skb->network_header - (unsigned int)(skb->data - skb->head)); }
static inline int skb_checksum_start_offset(const struct sk_buff *skb)
{ return (int)skb->csum_start; }

static inline void skb_checksum_none_assert(const struct sk_buff *skb) { (void)skb; }

/* GSO. */
static inline bool skb_is_gso(const struct sk_buff *skb) { return skb_shinfo(skb)->gso_size; }
static inline bool skb_is_gso_v6(const struct sk_buff *skb)
{ return skb_shinfo(skb)->gso_type & SKB_GSO_TCPV6; }

/* Подрезка/подтяжка (тела в lx_kit.c). */
int   pskb_trim(struct sk_buff *skb, unsigned int len);
void *__pskb_pull_tail(struct sk_buff *skb, int delta);
#define skb_tcp_all_headers(skb) \
	(skb_transport_offset(skb) + 20 /* упрощённо: L4-offset + типовой TCP-заголовок */)

/* Фрагменты (скаттер-гэзер). */
static inline struct page *skb_frag_page(const skb_frag_t *frag) { return frag->page; }
static inline unsigned int skb_frag_size(const skb_frag_t *frag) { return frag->size; }
static inline unsigned int skb_frag_off(const skb_frag_t *frag)  { return frag->offset; }
void skb_fill_page_desc(struct sk_buff *skb, int i, struct page *page, int off, int size);
dma_addr_t skb_frag_dma_map(struct device *dev, const skb_frag_t *frag,
			    size_t offset, size_t size, int dir);

void skb_tx_timestamp(struct sk_buff *skb);

/* Номер очереди, в которую стек направил пакет. Очередь у нас одна (см. alloc_etherdev_mq),
 * поэтому всегда нулевая — но функция обязана быть: драйвер по ней выбирает кольцо. */
static inline u16 skb_get_queue_mapping(const struct sk_buff *skb) { (void)skb; return 0; }

#endif /* _LINUX_SKBUFF_H_SHIM */
