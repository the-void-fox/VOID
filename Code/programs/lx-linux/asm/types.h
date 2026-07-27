/* asm/types.h — ШИМ lx_emul (Веха 58, обновлён Веха 68), НЕ исходник Linux.
 *
 * Фиксированные целые ядра (__u8..__u64/__s8..__s64) — в ядре из asm-generic/int-ll64.h. Теперь их
 * единый источник — <linux/types.h> (там же u8..u64 и endian-типы), чтобы 64-битные везде были
 * `long long` (совпадение с %ll в vendored-коде). Некоторый код ядра включает <asm/types.h> напрямую
 * (напр. lib/hweight.c) — отсюда просто переотсылаем к linux/types.h.
 */
#ifndef _ASM_TYPES_H_SHIM
#define _ASM_TYPES_H_SHIM

#include <linux/types.h>

#endif /* _ASM_TYPES_H_SHIM */
