/* linux/in.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. Номера IP-протоколов (для TX-offload). */
#ifndef _LINUX_IN_H_SHIM
#define _LINUX_IN_H_SHIM
#include <linux/types.h>
#define IPPROTO_IP   0
#define IPPROTO_TCP  6
#define IPPROTO_UDP  17
#define IPPROTO_IPV6 41
#endif /* _LINUX_IN_H_SHIM */
