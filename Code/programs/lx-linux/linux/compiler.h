/* linux/compiler.h — ШИМ lx_emul (Веха 57), НЕ исходник Linux.
 *
 * Обёртки компилятора, на которые опирается почти весь код ядра: подсказки предсказателю ветвлений
 * likely/unlikely, барьер компилятора, READ_ONCE/WRITE_ONCE и sparse-аннотации адресных пространств
 * (__user/__iomem/…). У нас gcc — likely/unlikely через __builtin_expect; sparse-аннотации пустые.
 * Растёт по мере надобности портируемого кода.
 */
#ifndef _LINUX_COMPILER_H_SHIM
#define _LINUX_COMPILER_H_SHIM

#ifndef likely
#define likely(x)   __builtin_expect(!!(x), 1)
#endif
#ifndef unlikely
#define unlikely(x) __builtin_expect(!!(x), 0)
#endif

/* Аннотации sparse (проверка адресных пространств) — у нас без sparse, пустые. */
#define __user
#define __iomem
#define __kernel
#define __force
#define __must_check
#define __percpu

#define barrier() __asm__ __volatile__("" : : : "memory")

#define READ_ONCE(x)     (*(const volatile typeof(x) *)&(x))
#define WRITE_ONCE(x, v) (*(volatile typeof(x) *)&(x) = (v))

#endif /* _LINUX_COMPILER_H_SHIM */
