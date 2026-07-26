/* linux/errno.h — ШИМ lx_emul (Веха 59), НЕ исходник Linux.
 *
 * Коды ошибок ядра (EINVAL/ENOMEM/ENODEV/EIO/…) — в ядре из asm-generic/errno*.h. У нас те же
 * имена и значения даёт newlib <errno.h>; тонкий проходной шим, чтобы код ядра, включающий
 * <linux/errno.h>, компилировался. Растёт по надобности (ERESTARTSYS и пр. — когда понадобятся).
 */
#ifndef _LINUX_ERRNO_H_SHIM
#define _LINUX_ERRNO_H_SHIM

#include <errno.h> /* newlib: EINVAL/ENOMEM/ENODEV/EIO/EBUSY/EAGAIN/… */

#endif /* _LINUX_ERRNO_H_SHIM */
