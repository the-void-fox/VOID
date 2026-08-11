/* linux/netdevice.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * Ядро сетевого стека — узкое подмножество под e1000: `net_device` + `net_device_ops`
 * (ndo_open/stop/start_xmit/…), очереди (netif_start/stop/wake_queue), NAPI (napi_struct/
 * napi_schedule/complete), статистика, фичи (NETIF_F_*). RX/TX замыкаются на наш net-srv
 * (Веха 34, [[virtio-net]]), а не на ядровый IP-стек. Функции объявлены, тела — в lx_kit.c.
 */
#ifndef _LINUX_NETDEVICE_H_SHIM
#define _LINUX_NETDEVICE_H_SHIM

#include <linux/types.h>
#include <linux/list.h>
#include <linux/atomic.h>
#include <linux/spinlock.h>
#include <linux/mutex.h>
#include <linux/sched.h>
#include <linux/timer.h>
#include <linux/workqueue.h>
#include <linux/interrupt.h>
#include <linux/skbuff.h>
#include <linux/device.h>

#define IFNAMSIZ      16
#define MAX_ADDR_LEN  32
#define ETH_ALEN      6

/* Сокет-адрес (ndo_set_mac_address получает struct sockaddr *). */
struct sockaddr {
	unsigned short sa_family;
	char           sa_data[14];
};

/* ── фичи устройства (netdev_features_t — битовое поле) ── */
typedef u64 netdev_features_t;
#define NETIF_F_SG                  (1ULL << 0)
#define NETIF_F_HW_CSUM             (1ULL << 1)
#define NETIF_F_RXCSUM             (1ULL << 2)
#define NETIF_F_HIGHDMA            (1ULL << 3)
#define NETIF_F_TSO                (1ULL << 4)
#define NETIF_F_RXALL              (1ULL << 5)
#define NETIF_F_RXFCS              (1ULL << 6)
#define NETIF_F_HW_VLAN_CTAG_TX    (1ULL << 7)
#define NETIF_F_HW_VLAN_CTAG_RX    (1ULL << 8)
#define NETIF_F_HW_VLAN_CTAG_FILTER (1ULL << 9)
#define NETIF_F_TSO6               (1ULL << 10)

/* ── флаги интерфейса ── */
#define IFF_UP           0x1
#define IFF_PROMISC      0x100
#define IFF_ALLMULTI     0x200
#define IFF_DOWN         0x0
#define IFF_SUPP_NOFCS   0x80000000
#define IFF_UNICAST_FLT  0x20000

/* ── уровни сообщений (netif_msg_*) ──
 *
 * Порядок бит — как в перечислении `netif_msg_class_bits` ядра (DRV=0 … WOL=14). Веха 131
 * выправила две записи: RX_STATUS и TX_DONE стояли МЕСТАМИ НАОБОРОТ, из-за чего гейт «показывать
 * состояние приёма» включал сообщения о завершении передачи и наоборот. На работу это не влияло —
 * только на то, что видно в журнале, — но именно поэтому и могло прожить сколько угодно. */
#define NETIF_MSG_DRV       0x0001
#define NETIF_MSG_PROBE     0x0002
#define NETIF_MSG_LINK      0x0004
#define NETIF_MSG_TIMER     0x0008
#define NETIF_MSG_IFDOWN    0x0010
#define NETIF_MSG_IFUP      0x0020
#define NETIF_MSG_RX_ERR    0x0040
#define NETIF_MSG_TX_ERR    0x0080
#define NETIF_MSG_TX_QUEUED 0x0100
#define NETIF_MSG_INTR      0x0200
#define NETIF_MSG_TX_DONE   0x0400
#define NETIF_MSG_RX_STATUS 0x0800
#define NETIF_MSG_PKTDATA   0x1000
#define NETIF_MSG_HW        0x2000
#define NETIF_MSG_WOL       0x4000

/* Откуда взялся аппаратный адрес (`net_device.addr_assign_type`). */
#define NET_ADDR_PERM   0
#define NET_ADDR_RANDOM 1
#define NET_ADDR_STOLEN 2
#define NET_ADDR_SET    3

/* ── результат передачи (ndo_start_xmit) ── */
typedef enum netdev_tx {
	NETDEV_TX_OK   = 0,
	NETDEV_TX_BUSY = 0x10,
} netdev_tx_t;

/* ── статистика ── */
struct net_device_stats {
	unsigned long rx_packets, tx_packets, rx_bytes, tx_bytes;
	unsigned long rx_errors, tx_errors, rx_dropped, tx_dropped;
	unsigned long multicast, collisions;
	unsigned long rx_length_errors, rx_over_errors, rx_crc_errors;
	unsigned long rx_frame_errors, rx_fifo_errors, rx_missed_errors;
	unsigned long tx_aborted_errors, tx_carrier_errors, tx_fifo_errors;
	unsigned long tx_heartbeat_errors, tx_window_errors;
};

/* ── список аппаратных адресов (mc/uc) ── */
struct netdev_hw_addr {
	struct list_head list;
	unsigned char    addr[MAX_ADDR_LEN];
};
struct netdev_hw_addr_list {
	struct list_head list;
	int              count;
};
#define netdev_for_each_mc_addr(ha, dev) \
	list_for_each_entry(ha, &(dev)->mc.list, list)
#define netdev_for_each_uc_addr(ha, dev) \
	list_for_each_entry(ha, &(dev)->uc.list, list)

/* ── NAPI ── */
#define NAPI_POLL_WEIGHT 64
struct napi_struct {
	struct list_head   poll_list;
	unsigned long      state;
	int                weight;
	int              (*poll)(struct napi_struct *, int);
	struct net_device *dev;
	unsigned int       napi_id;
	/* приватная линковка Lx_kit */
	void              *lx_priv;
};

struct ifreq; /* eth_ioctl */

struct net_device_ops {
	int         (*ndo_open)(struct net_device *dev);
	int         (*ndo_stop)(struct net_device *dev);
	netdev_tx_t (*ndo_start_xmit)(struct sk_buff *skb, struct net_device *dev);
	void        (*ndo_set_rx_mode)(struct net_device *dev);
	struct net_device_stats *(*ndo_get_stats)(struct net_device *dev);
	int         (*ndo_set_mac_address)(struct net_device *dev, void *addr);
	int         (*ndo_validate_addr)(struct net_device *dev);
	int         (*ndo_change_mtu)(struct net_device *dev, int new_mtu);
	int         (*ndo_eth_ioctl)(struct net_device *dev, struct ifreq *ifr, int cmd);
	void        (*ndo_tx_timeout)(struct net_device *dev, unsigned int txqueue);
	int         (*ndo_vlan_rx_add_vid)(struct net_device *dev, __be16 proto, u16 vid);
	int         (*ndo_vlan_rx_kill_vid)(struct net_device *dev, __be16 proto, u16 vid);
	void        (*ndo_poll_controller)(struct net_device *dev);
	netdev_features_t (*ndo_fix_features)(struct net_device *dev, netdev_features_t features);
	int         (*ndo_set_features)(struct net_device *dev, netdev_features_t features);
};

struct net_device {
	char                       name[IFNAMSIZ];
	unsigned int               flags;
	unsigned int               priv_flags;
	unsigned int               mtu;
	unsigned int               min_mtu;
	unsigned int               max_mtu;
	unsigned char              addr_len;
	unsigned char              addr_assign_type; /* NET_ADDR_* — откуда взялся адрес */
	unsigned char              dev_addr[MAX_ADDR_LEN];
	netdev_features_t          features;
	netdev_features_t          hw_features;
	netdev_features_t          vlan_features;
	int                        watchdog_timeo;
	const struct net_device_ops *netdev_ops;
	const struct ethtool_ops    *ethtool_ops;
	struct net_device_stats    stats;
	struct netdev_hw_addr_list mc;
	struct netdev_hw_addr_list uc;
	unsigned int               irq;
	struct device              dev;    /* базовый узел (SET_NETDEV_DEV, &netdev->dev) */
	void                      *lx_priv; /* приватная область драйвера (netdev_priv) */
	unsigned long              lx_state;
/* Биты `lx_state` (Веха 134.3). Пока один: несущая. Ведём её честно — заглушка «несущая есть
 * всегда» уже соврала один раз в прогоне, где `netif_carrier_on` не звался вовсе. */
#define LX_STATE_CARRIER 0
	/* Веха 131 — ОДНА очередь передачи, объектом. Драйверы многоочередных карт (atl1c)
	 * работают не с устройством, а с `netdev_queue`, и в Linux их столько, сколько запросил
	 * `alloc_etherdev_mq`. У нас очередь одна: и AR8151, и e1000 просят ровно одну
	 * (четыре — только у варианта MediaTek, которого в X54C нет). Больше одной честно
	 * отвергаем при создании, а не делаем вид, что справились. */
	struct netdev_queue       *lx_txq;
	unsigned int               lx_num_tx_queues;
};

static inline void *netdev_priv(const struct net_device *dev) { return dev->lx_priv; }
#define SET_NETDEV_DEV(net, pdev) ((net)->dev.parent = (pdev))
#define netdev_uc_count(dev) ((dev)->uc.count)

/* Регистрация (тела в lx_kit.c). */
int  register_netdev(struct net_device *dev);
void unregister_netdev(struct net_device *dev);
void free_netdev(struct net_device *dev);

/* NAPI (тела в lx_kit.c). */
void netif_napi_add(struct net_device *dev, struct napi_struct *napi,
		    int (*poll)(struct napi_struct *, int));
void netif_napi_set_irq(struct napi_struct *napi, int irq);
void netif_queue_set_napi(struct net_device *dev, unsigned int qidx,
			  int type, struct napi_struct *napi);
void napi_enable(struct napi_struct *napi);
void napi_disable(struct napi_struct *napi);
void __napi_schedule(struct napi_struct *napi);
bool napi_schedule_prep(struct napi_struct *napi);
bool napi_complete_done(struct napi_struct *napi, int work_done);
void napi_gro_receive(struct napi_struct *napi, struct sk_buff *skb);
static inline void napi_schedule(struct napi_struct *napi)
{ if (napi_schedule_prep(napi)) __napi_schedule(napi); }

/* Типы очередей для netif_queue_set_napi. */
enum netdev_queue_type { NETDEV_QUEUE_TYPE_RX, NETDEV_QUEUE_TYPE_TX };

/* Очереди TX (тела в lx_kit.c / inline). */
void netif_start_queue(struct net_device *dev);
void netif_stop_queue(struct net_device *dev);
void netif_wake_queue(struct net_device *dev);
void netif_tx_disable(struct net_device *dev);
bool netif_queue_stopped(const struct net_device *dev);
bool netif_running(const struct net_device *dev);
void netif_carrier_on(struct net_device *dev);
void netif_carrier_off(struct net_device *dev);
bool netif_carrier_ok(const struct net_device *dev);
void netif_device_attach(struct net_device *dev);
void netif_device_detach(struct net_device *dev);

/* ── очередь передачи (Веха 131) ──
 *
 * Одна на устройство, но ОБЪЕКТОМ: драйвер многоочередной карты останавливает и будит именно
 * очередь, а не устройство целиком. Свести это к устройству — не потеря: очередь у нас одна,
 * и её состояние и есть состояние устройства.
 *
 * BQL (byte queue limits) остаётся заглушкой: это регулятор глубины буфера против bufferbloat,
 * а не условие работы карты. Заводить его имеет смысл, когда появится очередь пакетов, которую
 * есть чем переполнять. */
struct netdev_queue {
	struct net_device *dev;
};

struct netdev_queue *netdev_get_tx_queue(struct net_device *dev, unsigned int index);

void netif_tx_stop_queue(struct netdev_queue *q);
void netif_tx_wake_queue(struct netdev_queue *q);
bool netif_tx_queue_stopped(const struct netdev_queue *q);

static inline void netdev_sent_queue(struct net_device *dev, unsigned int bytes) { (void)dev; (void)bytes; }
static inline void netdev_completed_queue(struct net_device *dev, unsigned int pkts, unsigned int bytes)
{ (void)dev; (void)pkts; (void)bytes; }
static inline void netdev_reset_queue(struct net_device *dev) { (void)dev; }
/* Возвращает «пора дёргать железо». В Linux это ЛОЖЬ при xmit_more (пакет кладут в пачку и
 * звонок откладывают) и ИСТИНА иначе. Тип возврата здесь не мелочь: верни мы void или всегда
 * false — драйвер не дёрнул бы TX-регистр ни разу, кольцо заполнялось бы, а карта молчала. */
static inline bool __netdev_tx_sent_queue(struct netdev_queue *q, unsigned int bytes, bool xmit_more)
{ (void)q; (void)bytes; return !xmit_more; }
static inline void netdev_tx_sent_queue(struct netdev_queue *q, unsigned int bytes)
{ (void)q; (void)bytes; }
static inline void netdev_tx_completed_queue(struct netdev_queue *q, unsigned int pkts, unsigned int bytes)
{ (void)q; (void)pkts; (void)bytes; }
static inline void netdev_tx_reset_queue(struct netdev_queue *q) { (void)q; }
static inline bool netif_xmit_stopped(const struct netdev_queue *q) { return netif_tx_queue_stopped(q); }
static inline bool netdev_xmit_more(void) { return false; }

/* Пересчитать активные фичи из hw_features/wanted. У нас фичи выставляет сам драйвер и никто
 * их не оспаривает, поэтому пересчитывать нечего — но позвать драйвер имеет право. */
void netdev_update_features(struct net_device *dev);

/* NAPI для ПЕРЕДАЧИ (netif_napi_add_tx) и нить опроса (netif_threaded_enable): в Linux это
 * разные контексты, у нас — тот же кооперативный планировщик Lx_kit. */
void netif_napi_add_tx(struct net_device *dev, struct napi_struct *napi,
		       int (*poll)(struct napi_struct *, int));
int  netif_threaded_enable(struct net_device *dev);

/* Сообщения уровня устройства → printk (наш device.h даёт dev_*). */
#define netdev_err(dev, fmt, ...)    printk(fmt, ##__VA_ARGS__)
#define netdev_warn(dev, fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define netdev_info(dev, fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define netdev_notice(dev, fmt, ...) printk(fmt, ##__VA_ARGS__)
#define netdev_dbg(dev, fmt, ...)    printk(fmt, ##__VA_ARGS__)

static inline u32 netif_msg_init(int debug_value, int default_msg_enable_bits)
{ return debug_value < 0 ? (u32)default_msg_enable_bits : (u32)debug_value; }

/* Гейты по классу сообщения: priv->msg_enable & NETIF_MSG_<TYPE> (priv = e1000_adapter). */
#define netif_msg_drv(p)        ((p)->msg_enable & NETIF_MSG_DRV)
#define netif_msg_probe(p)      ((p)->msg_enable & NETIF_MSG_PROBE)
#define netif_msg_link(p)       ((p)->msg_enable & NETIF_MSG_LINK)
#define netif_msg_ifdown(p)     ((p)->msg_enable & NETIF_MSG_IFDOWN)
#define netif_msg_ifup(p)       ((p)->msg_enable & NETIF_MSG_IFUP)
#define netif_msg_rx_err(p)     ((p)->msg_enable & NETIF_MSG_RX_ERR)
#define netif_msg_tx_err(p)     ((p)->msg_enable & NETIF_MSG_TX_ERR)
#define netif_msg_tx_queued(p)  ((p)->msg_enable & NETIF_MSG_TX_QUEUED)
#define netif_msg_tx_done(p)    ((p)->msg_enable & NETIF_MSG_TX_DONE)
#define netif_msg_rx_status(p)  ((p)->msg_enable & NETIF_MSG_RX_STATUS)
#define netif_msg_pktdata(p)    ((p)->msg_enable & NETIF_MSG_PKTDATA)
#define netif_msg_intr(p)       ((p)->msg_enable & NETIF_MSG_INTR)
#define netif_msg_timer(p)      ((p)->msg_enable & NETIF_MSG_TIMER)
#define netif_msg_wol(p)        ((p)->msg_enable & NETIF_MSG_WOL)
#define netif_msg_hw(p)         ((p)->msg_enable & NETIF_MSG_HW)
#define netif_msg_rx_status(p)  ((p)->msg_enable & NETIF_MSG_RX_STATUS)
#define netif_msg_tx_done(p)    ((p)->msg_enable & NETIF_MSG_TX_DONE)

/* netif_<level>(priv, type, dev, fmt…) — печать под гейтом класса (тип потребляется гейтом). */
#define netif_level(level, priv, type, dev, fmt, ...) \
	do { if (netif_msg_##type(priv)) netdev_##level(dev, fmt, ##__VA_ARGS__); } while (0)
#define netif_err(priv, type, dev, fmt, ...)    netif_level(err, priv, type, dev, fmt, ##__VA_ARGS__)
#define netif_warn(priv, type, dev, fmt, ...)   netif_level(warn, priv, type, dev, fmt, ##__VA_ARGS__)
#define netif_info(priv, type, dev, fmt, ...)   netif_level(info, priv, type, dev, fmt, ##__VA_ARGS__)
#define netif_notice(priv, type, dev, fmt, ...) netif_level(notice, priv, type, dev, fmt, ##__VA_ARGS__)
#define netif_dbg(priv, type, dev, fmt, ...)    netif_level(dbg, priv, type, dev, fmt, ##__VA_ARGS__)

static inline int net_ratelimit(void) { return 1; }

/* NAPI-фрагменты (jumbo-RX через страницы) — тела в lx_kit.c. */
struct sk_buff *napi_get_frags(struct napi_struct *napi);
void napi_free_frags(struct napi_struct *napi);
int  napi_gro_frags(struct napi_struct *napi);

/* rtnl — в один-поток-модели глобальной блокировки конфигурации не нужно (no-op). */
static inline void rtnl_lock(void)   { }
static inline void rtnl_unlock(void) { }

#endif /* _LINUX_NETDEVICE_H_SHIM */
