/* linux/bitfield.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * FIELD_GET/FIELD_PREP — извлечение/упаковка поля по маске (сдвиг = позиция младшего бита маски). */
#ifndef _LINUX_BITFIELD_H_SHIM
#define _LINUX_BITFIELD_H_SHIM

#include <linux/types.h>

#define __bf_shf(x) (__builtin_ffsll(x) - 1)

#define FIELD_PREP(_mask, _val) \
	(((typeof(_mask))(_val) << __bf_shf(_mask)) & (_mask))

#define FIELD_GET(_mask, _reg) \
	((typeof(_mask))(((_reg) & (_mask)) >> __bf_shf(_mask)))

#define FIELD_FIT(_mask, _val) \
	(!((((typeof(_mask))(_val)) << __bf_shf(_mask)) & ~(_mask)))

#endif /* _LINUX_BITFIELD_H_SHIM */
