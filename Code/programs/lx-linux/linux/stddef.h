/* linux/stddef.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * offsetof/NULL + булевы имена ядра. */
#ifndef _LINUX_STDDEF_H_SHIM
#define _LINUX_STDDEF_H_SHIM

#include <linux/types.h>
#include <stddef.h> /* offsetof, NULL, size_t */

#ifndef sizeof_field
#define sizeof_field(TYPE, MEMBER) (sizeof(((TYPE *)0)->MEMBER))
#endif
#ifndef offsetofend
#define offsetofend(TYPE, MEMBER) (offsetof(TYPE, MEMBER) + sizeof_field(TYPE, MEMBER))
#endif

/* struct_group — группа полей под общим именем (ядро 5.16+); нам хватает прозрачной раскладки. */
#define struct_group(NAME, ...) \
	union { struct { __VA_ARGS__ }; struct { __VA_ARGS__ } NAME; }

#endif /* _LINUX_STDDEF_H_SHIM */
