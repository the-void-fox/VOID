/* linux/udp.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. UDP-заголовок (checksum-offload). */
#ifndef _LINUX_UDP_H_SHIM
#define _LINUX_UDP_H_SHIM
#include <linux/types.h>

struct udphdr {
	__be16 source;
	__be16 dest;
	__be16 len;
	__sum16 check;
};

#endif /* _LINUX_UDP_H_SHIM */
