/* linux/vmalloc.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * vmalloc/vzalloc/vfree — у нас единая куча newlib (виртуально-непрерывная и так). */
#ifndef _LINUX_VMALLOC_H_SHIM
#define _LINUX_VMALLOC_H_SHIM

#include <linux/types.h>

void *vmalloc(unsigned long size);
void *vzalloc(unsigned long size);
void  vfree(const void *addr);

#endif /* _LINUX_VMALLOC_H_SHIM */
