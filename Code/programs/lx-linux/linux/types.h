/* linux/types.h — ШИМ lx_emul (Веха 55), НЕ исходник Linux.
 *
 * Первый кусок «lx_emul-заголовков» (Genode dde_linux-стиль): даёт неизменённому коду ядра Linux
 * ровно те типы/макросы, что он ждёт от <linux/types.h>, но реализованные под VOID (поверх newlib
 * stdint/stddef/stdbool). Здесь — минимум под lib/sort.c; растёт по мере роста портируемого кода.
 * В настоящем ядре часть этого приходит транзитивно из <linux/compiler.h> — здесь собрано вместе.
 */
#ifndef _LINUX_TYPES_H_SHIM
#define _LINUX_TYPES_H_SHIM

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
typedef int8_t s8;
typedef int16_t s16;
typedef int32_t s32;
typedef int64_t s64;

/* uapi-имена тех же фикс-типов (в ядре — из <asm-generic/int-ll64.h>). */
typedef u8  __u8;
typedef u16 __u16;
typedef u32 __u32;
typedef u64 __u64;
typedef s8  __s8;
typedef s16 __s16;
typedef s32 __s32;
typedef s64 __s64;

/* Атрибуты компилятора (в ядре — из <linux/compiler.h>/compiler_attributes.h). */
#ifndef __always_inline
#define __always_inline inline __attribute__((always_inline))
#endif
#ifndef __attribute_const__
#define __attribute_const__ __attribute__((__const__))
#endif

/* Колбэки sort/bsearch — как в настоящем <linux/types.h> (добавлены в ядро ~5.16). */
typedef int (*cmp_func_t)(const void *a, const void *b);
typedef int (*cmp_r_func_t)(const void *a, const void *b, const void *priv);
typedef void (*swap_func_t)(void *a, void *b, int size);
typedef void (*swap_r_func_t)(void *a, void *b, int size, const void *priv);

#endif /* _LINUX_TYPES_H_SHIM */
