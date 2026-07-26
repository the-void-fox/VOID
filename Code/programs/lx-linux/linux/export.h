/* linux/export.h — ШИМ lx_emul (Веха 55), НЕ исходник Linux.
 *
 * EXPORT_SYMBOL в ядре публикует символ для модулей; у нас всё линкуется статически в один ELF —
 * экспортировать некому, макросы пустые. Символ и так виден линкеру внутри программы.
 */
#ifndef _LINUX_EXPORT_H_SHIM
#define _LINUX_EXPORT_H_SHIM

#define EXPORT_SYMBOL(sym)
#define EXPORT_SYMBOL_GPL(sym)
#define EXPORT_SYMBOL_NS(sym, ns)
#define EXPORT_SYMBOL_NS_GPL(sym, ns)

#endif /* _LINUX_EXPORT_H_SHIM */
