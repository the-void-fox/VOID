/* linux/capability.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * POSIX-caps ядра. У нас один доверенный драйвер → capable() всегда истина. */
#ifndef _LINUX_CAPABILITY_H_SHIM
#define _LINUX_CAPABILITY_H_SHIM

#include <linux/types.h>

#define CAP_NET_ADMIN 12

static inline bool capable(int cap) { (void)cap; return true; }

#endif /* _LINUX_CAPABILITY_H_SHIM */
