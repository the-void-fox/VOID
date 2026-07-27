/* net/ip6_checksum.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Псевдозаголовочная сумма IPv6 (e1000 считает частичную для HW-offload TCP/UDP over IPv6). */
#ifndef _NET_IP6_CHECKSUM_H_SHIM
#define _NET_IP6_CHECKSUM_H_SHIM

#include <linux/types.h>
#include <linux/ipv6.h>
#include <linux/skbuff.h>
#include <net/checksum.h>

/* Подготовка частичной суммы для TCP-over-IPv6 GSO/offload (тело в lx_kit.c). */
void tcp_v6_gso_csum_prep(struct sk_buff *skb);

static inline __sum16 csum_ipv6_magic(const struct in6_addr *saddr,
				      const struct in6_addr *daddr,
				      __u32 len, __u8 proto, __wsum csum)
{
	u64 s = (u32)csum;
	int i;
	for (i = 0; i < 4; i++) {
		s += (u32)saddr->s6_addr32[i];
		s += (u32)daddr->s6_addr32[i];
	}
	s += cpu_to_be32(len);
	s += cpu_to_be32(proto);
	s = (s & 0xffffffff) + (s >> 32);
	s = (s & 0xffffffff) + (s >> 32);
	return csum_fold((__wsum)s);
}

#endif /* _NET_IP6_CHECKSUM_H_SHIM */
