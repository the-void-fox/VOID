/* asm/byteorder.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * Конверсии порядка байт. Обе наши арх (riscv64/x86_64) — little-endian, поэтому cpu↔le —
 * тождества, cpu↔be — байт-своп. Драйверу нужны и типы `__leNN`/`__beNN` (из linux/types.h).
 */
#ifndef _ASM_BYTEORDER_H_SHIM
#define _ASM_BYTEORDER_H_SHIM

#include <linux/types.h>

#define __LITTLE_ENDIAN 1234
#ifndef __BYTE_ORDER
#define __BYTE_ORDER __LITTLE_ENDIAN
#endif

/* __builtin_bswap* — константно-складываемы (годны для case-меток типа cpu_to_be16(ETH_P_IP)). */
#define swab16(x) __builtin_bswap16((u16)(x))
#define swab32(x) __builtin_bswap32((u32)(x))
#define swab64(x) __builtin_bswap64((u64)(x))

/* LE → тождества (наши арх LE). */
#define cpu_to_le16(x) ((__le16)(u16)(x))
#define cpu_to_le32(x) ((__le32)(u32)(x))
#define cpu_to_le64(x) ((__le64)(u64)(x))
#define le16_to_cpu(x) ((u16)(__le16)(x))
#define le32_to_cpu(x) ((u32)(__le32)(x))
#define le64_to_cpu(x) ((u64)(__le64)(x))
#define le16_to_cpus(p) do { } while (0)
#define le32_to_cpus(p) do { } while (0)

/* BE → байт-своп (константно-складываемо). */
#define cpu_to_be16(x) ((__be16)__builtin_bswap16((u16)(x)))
#define cpu_to_be32(x) ((__be32)__builtin_bswap32((u32)(x)))
#define cpu_to_be64(x) ((__be64)__builtin_bswap64((u64)(x)))
#define be16_to_cpu(x) (__builtin_bswap16((u16)(__be16)(x)))
#define be32_to_cpu(x) (__builtin_bswap32((u32)(__be32)(x)))
#define be64_to_cpu(x) (__builtin_bswap64((u64)(__be64)(x)))
#define ntohs(x) be16_to_cpu(x)
#define ntohl(x) be32_to_cpu(x)
#define htons(x) cpu_to_be16(x)
#define htonl(x) cpu_to_be32(x)

#endif /* _ASM_BYTEORDER_H_SHIM */
