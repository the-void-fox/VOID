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

#endif /* _LINUX_KERNEL_H_SHIM */
