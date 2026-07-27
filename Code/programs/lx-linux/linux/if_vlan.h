/* linux/if_vlan.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. VLAN-хелперы поверх netdev/skb. */
#ifndef _LINUX_IF_VLAN_H_SHIM
#define _LINUX_IF_VLAN_H_SHIM

#include <linux/types.h>
#include <linux/skbuff.h>
#include <linux/etherdevice.h>

#define VLAN_N_VID         4096
#define VLAN_PRIO_MASK     0xe000
#define VLAN_VID_MASK      0x0fff
#define VLAN_HLEN          4
#define VLAN_ETH_FRAME_LEN 1518

/* skb с VLAN-тегом: у нас offload VLAN пока не задействован → тега нет. */
static inline bool skb_vlan_tag_present(const struct sk_buff *skb) { (void)skb; return false; }
static inline u16  skb_vlan_tag_get(const struct sk_buff *skb) { (void)skb; return 0; }

/* Протокол L3 из skb (при выключенном VLAN-offload = skb->protocol). */
static inline __be16 vlan_get_protocol(const struct sk_buff *skb) { return skb->protocol; }
/* Аппаратная вставка VLAN-тега — тело в lx_kit.c (пока фактически no-op). */
void __vlan_hwaccel_put_tag(struct sk_buff *skb, __be16 vlan_proto, u16 vlan_tci);

#endif /* _LINUX_IF_VLAN_H_SHIM */
