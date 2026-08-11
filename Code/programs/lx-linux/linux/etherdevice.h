/* linux/etherdevice.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Ethernet-хелперы поверх netdev/skb: alloc_etherdev, проверка/копирование MAC, eth_type_trans. */
#ifndef _LINUX_ETHERDEVICE_H_SHIM
#define _LINUX_ETHERDEVICE_H_SHIM

#include <linux/types.h>
#include <linux/netdevice.h>
#include <linux/skbuff.h>
#include <linux/string.h>

#define ETH_HLEN      14
#define ETH_FCS_LEN   4
#define ETH_ZLEN      60
#define ETH_DATA_LEN  1500
#define ETH_FRAME_LEN 1514
#define ETH_P_IP      0x0800
#define ETH_P_IPV6    0x86DD
#define ETH_P_8021Q   0x8100

struct net_device *alloc_etherdev(int sizeof_priv);
__be16 eth_type_trans(struct sk_buff *skb, struct net_device *dev);
int  eth_validate_addr(struct net_device *dev);

static inline bool is_zero_ether_addr(const u8 *addr)
{ return !(addr[0] | addr[1] | addr[2] | addr[3] | addr[4] | addr[5]); }
static inline bool is_multicast_ether_addr(const u8 *addr) { return addr[0] & 1; }
static inline bool is_broadcast_ether_addr(const u8 *addr)
{ return (addr[0] & addr[1] & addr[2] & addr[3] & addr[4] & addr[5]) == 0xff; }
static inline bool is_valid_ether_addr(const u8 *addr)
{ return !is_multicast_ether_addr(addr) && !is_zero_ether_addr(addr); }
static inline void eth_broadcast_addr(u8 *addr) { memset(addr, 0xff, ETH_ALEN); }
static inline void eth_zero_addr(u8 *addr) { memset(addr, 0x00, ETH_ALEN); }
static inline void ether_addr_copy(u8 *dst, const u8 *src) { memcpy(dst, src, ETH_ALEN); }
static inline bool ether_addr_equal(const u8 *a, const u8 *b) { return memcmp(a, b, ETH_ALEN) == 0; }
static inline void eth_hw_addr_set(struct net_device *dev, const u8 *addr)
{ memcpy(dev->dev_addr, addr, ETH_ALEN); }
/* Случайный локально-администрируемый одноадресный MAC. Карта без адреса в EEPROM обязана
 * выдумать себе адрес — бит 1 первого байта помечает его как «назначенный локально», бит 0
 * снятый делает его одноадресным (Веха 131). */
void eth_random_addr(u8 *addr);
void eth_hw_addr_random(struct net_device *dev);

#endif /* _LINUX_ETHERDEVICE_H_SHIM */
