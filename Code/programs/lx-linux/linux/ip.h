/* linux/ip.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. IPv4-заголовок (парсинг при TX-offload). */
#ifndef _LINUX_IP_H_SHIM
#define _LINUX_IP_H_SHIM
#include <linux/types.h>

struct iphdr {
	__u8    ihl:4,
		version:4;
	__u8    tos;
	__be16  tot_len;
	__be16  id;
	__be16  frag_off;
	__u8    ttl;
	__u8    protocol;
	__sum16 check;
	__be32  saddr;
	__be32  daddr;
};

#include <linux/skbuff.h>
static inline struct iphdr *ip_hdr(const struct sk_buff *skb)
{ return (struct iphdr *)skb_network_header(skb); }

#endif /* _LINUX_IP_H_SHIM */
