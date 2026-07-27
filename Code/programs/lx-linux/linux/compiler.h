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

/* Атрибуты размещения в секциях — у нас без спец-секций, пустые. Часть (__packed/__aligned)
 * newlib уже даёт в <sys/cdefs.h> — определяем только недостающее (#ifndef). */
#define __read_mostly
#define __cold
#define __ro_after_init
#define ____cacheline_aligned
#ifndef __aligned
#define __aligned(n) __attribute__((aligned(n)))
#endif
#ifndef __maybe_unused
#define __maybe_unused __attribute__((unused))
#endif
#ifndef __always_unused
#define __always_unused __attribute__((unused))
#endif
#ifndef __packed
#define __packed __attribute__((packed))
#endif

#ifndef barrier
#define barrier() __asm__ __volatile__("" : : : "memory")
#endif

/* Явный проброс в switch (ядро 5.x+) — атрибут компилятора вместо комментария. */
#ifndef fallthrough
#define fallthrough __attribute__((__fallthrough__))
#endif

#define READ_ONCE(x)     (*(const volatile typeof(x) *)&(x))
#define WRITE_ONCE(x, v) (*(volatile typeof(x) *)&(x) = (v))

#endif /* _LINUX_COMPILER_H_SHIM */
