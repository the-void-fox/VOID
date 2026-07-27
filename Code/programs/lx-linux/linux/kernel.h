/* linux/kernel.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * «Зонтик» общих идиом ядра. В настоящем ядре тянет десятки заголовков; нам нужен компактный
 * набор макросов, на который опирается портируемый код (container_of, min/max, ARRAY_SIZE,
 * округления/выравнивание, swap). Печать — в <linux/printk.h>, аллокатор — в <linux/slab.h>,
 * строки — в <linux/string.h>. Растёт по мере надобности портируемых файлов.
 */
#ifndef _LINUX_KERNEL_H_SHIM
#define _LINUX_KERNEL_H_SHIM

#include <linux/container_of.h> /* container_of() — базовая идиома, вынесена отдельно */
#include <linux/types.h>
#include <linux/compiler.h>     /* likely/unlikely/fallthrough/READ_ONCE — нужны почти всему коду */

#define ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))

#define min(a, b) ((a) < (b) ? (a) : (b))
#define max(a, b) ((a) > (b) ? (a) : (b))
#define min3(a, b, c) min(min(a, b), c)
#define max3(a, b, c) max(max(a, b), c)
#define clamp(v, lo, hi) max((lo), min((v), (hi)))

#define swap(a, b) do { typeof(a) __swap_tmp = (a); (a) = (b); (b) = __swap_tmp; } while (0)

#define DIV_ROUND_UP(n, d) (((n) + (d) - 1) / (d))
#define ALIGN(x, a)      (((x) + ((typeof(x))(a) - 1)) & ~((typeof(x))(a) - 1))
#define round_up(x, a)   ALIGN((x), (a))
#define round_down(x, a) ((x) & ~((typeof(x))(a) - 1))

#define min_t(type, a, b) ((type)(a) < (type)(b) ? (type)(a) : (type)(b))
#define max_t(type, a, b) ((type)(a) > (type)(b) ? (type)(a) : (type)(b))
#define clamp_t(type, v, lo, hi) max_t(type, (lo), min_t(type, (v), (hi)))

#define upper_32_bits(n) ((u32)(((n) >> 16) >> 16))
#define lower_32_bits(n) ((u32)((n) & 0xffffffff))

/* Дамп в hex — отладочная печать буфера (тело в lx_kit.c). */
#define DUMP_PREFIX_NONE    0
#define DUMP_PREFIX_ADDRESS 1
#define DUMP_PREFIX_OFFSET  2
void print_hex_dump(const char *level, const char *prefix_str, int prefix_type,
		    int rowsize, int groupsize, const void *buf, size_t len, bool ascii);

/* Стадия системы (e1000 в shutdown отличает выключение). Значение — в lx_kit.c. */
enum system_states { SYSTEM_BOOTING, SYSTEM_RUNNING, SYSTEM_HALT, SYSTEM_POWER_OFF, SYSTEM_RESTART };
extern enum system_states system_state;

/* Диагностика: WARN_ON печатает и возвращает условие; BUG_ON — фатально. */
#define WARN_ON(cond)      ({ int __c = !!(cond); if (__c) printk("WARN_ON: %s\n", #cond); __c; })
#define WARN_ON_ONCE(cond) WARN_ON(cond)
#define WARN(cond, fmt, ...) ({ int __c = !!(cond); if (__c) printk(fmt, ##__VA_ARGS__); __c; })
#define BUG_ON(cond)       do { if (cond) { printk("BUG: %s\n", #cond); for (;;) ; } } while (0)
#define BUG()              do { printk("BUG at %s:%d\n", __FILE__, __LINE__); for (;;) ; } while (0)

#endif /* _LINUX_KERNEL_H_SHIM */
