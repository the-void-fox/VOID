/* linux/ipv6.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. IPv6-заголовок (парсинг при TX-offload). */
#ifndef _LINUX_IPV6_H_SHIM
#define _LINUX_IPV6_H_SHIM
#include <linux/types.h>

struct in6_addr {
	union {
		__u8  u6_addr8[16];
		__be16 u6_addr16[8];
		__be32 u6_addr32[4];
	} in6_u;
#define s6_addr   in6_u.u6_addr8
#define s6_addr16 in6_u.u6_addr16
#define s6_addr32 in6_u.u6_addr32
};

struct ipv6hdr {
	__u8    priority:4,
		version:4;
	__u8    flow_lbl[3];
	__be16  payload_len;
	__u8    nexthdr;
	__u8    hop_limit;
	struct in6_addr saddr;
	struct in6_addr daddr;
};

#include <linux/skbuff.h>
static inline struct ipv6hdr *ipv6_hdr(const struct sk_buff *skb)
{ return (struct ipv6hdr *)skb_network_header(skb); }

#endif /* _LINUX_IPV6_H_SHIM */
