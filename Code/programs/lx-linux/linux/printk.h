/* linux/printk.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * printk() — журнал ядра. В Lx_kit печатаем через newlib (vprintf → stdout VOID → консоль).
 * Уровни KERN_* в ядре — спец-первый-байт строки; у нас лог один, префиксы-пустышки. pr_* —
 * привычные обёртки. Достаточно, чтобы неизменённый код ядра и наш харнесс печатали как в Linux.
 */
#ifndef _LINUX_PRINTK_H_SHIM
#define _LINUX_PRINTK_H_SHIM

#define KERN_EMERG   ""
#define KERN_ALERT   ""
#define KERN_CRIT    ""
#define KERN_ERR     ""
#define KERN_WARNING ""
#define KERN_NOTICE  ""
#define KERN_INFO    ""
#define KERN_DEBUG   ""
#define KERN_CONT    ""

__attribute__((format(printf, 1, 2))) int printk(const char *fmt, ...);

#define pr_fmt(fmt) fmt
#define pr_info(fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define pr_err(fmt, ...)    printk(fmt, ##__VA_ARGS__)
#define pr_warn(fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define pr_notice(fmt, ...) printk(fmt, ##__VA_ARGS__)
#define pr_debug(fmt, ...)  printk(fmt, ##__VA_ARGS__)
#define pr_cont(fmt, ...)   printk(fmt, ##__VA_ARGS__)

#endif /* _LINUX_PRINTK_H_SHIM */
