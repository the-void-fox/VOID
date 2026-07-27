/* net/checksum.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Псевдозаголовочные контрольные суммы (e1000 считает частичную сумму для HW-offload).
 * Реализация софтовая; при выключенном offload почти не вызывается. */
#ifndef _NET_CHECKSUM_H_SHIM
#define _NET_CHECKSUM_H_SHIM

#include <linux/types.h>
#include <asm/byteorder.h>

/* Свёртка 32-битной суммы в 16 бит (стандартный приём). */
static inline __sum16 csum_fold(__wsum csum)
{
	u32 sum = (u32)csum;
	sum = (sum & 0xffff) + (sum >> 16);
	sum = (sum & 0xffff) + (sum >> 16);
	return (__sum16)~sum;
}

static inline __wsum csum_tcpudp_nofold(__be32 saddr, __be32 daddr,
					__u32 len, __u8 proto, __wsum sum)
{
	u64 s = (u32)sum;
	s += (u32)saddr;
	s += (u32)daddr;
	s += cpu_to_be32(len + proto);
	s = (s & 0xffffffff) + (s >> 32);
	return (__wsum)((s & 0xffffffff) + (s >> 32));
}

static inline __sum16 csum_tcpudp_magic(__be32 saddr, __be32 daddr,
					__u32 len, __u8 proto, __wsum sum)
{
	return csum_fold(csum_tcpudp_nofold(saddr, daddr, len, proto, sum));
}

static inline __wsum csum_partial(const void *buff, int len, __wsum sum)
{
	const u8 *p = buff;
	u32 s = (u32)sum;
	int i;
	for (i = 0; i + 1 < len; i += 2)
		s += (u32)((p[i] << 8) | p[i + 1]);
	if (i < len)
		s += (u32)(p[i] << 8);
	return (__wsum)s;
}

#endif /* _NET_CHECKSUM_H_SHIM */
