/* asm/types.h — ШИМ lx_emul (Веха 58), НЕ исходник Linux.
 *
 * Фиксированные целые ядра с ДВОЙНЫМ подчёркиванием (__u8..__u64/__s8..__s64) — в ядре приходят из
 * asm-generic/int-ll64.h. Даём их поверх newlib <stdint.h>. Некоторый код ядра (напр. lib/hweight.c)
 * включает <asm/types.h> напрямую. Короткие имена u8..u64 — в <linux/types.h>.
 */
#ifndef _ASM_TYPES_H_SHIM
#define _ASM_TYPES_H_SHIM

#include <stdint.h>

typedef uint8_t  __u8;
typedef uint16_t __u16;
typedef uint32_t __u32;
typedef uint64_t __u64;
typedef int8_t   __s8;
typedef int16_t  __s16;
typedef int32_t  __s32;
typedef int64_t  __s64;

#endif /* _ASM_TYPES_H_SHIM */
