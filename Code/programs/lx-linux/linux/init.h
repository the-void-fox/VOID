/* linux/init.h — ШИМ lx_emul (Веха 193), НЕ исходник Linux.
 *
 * Пометки секций (`__init`, `__exit`, `__initdata`) и регистрация модуля. У нас нет ни секций
 * инициализации, ни выгрузки модулей: драйвер — это процесс, который живёт, пока живёт карта.
 * Поэтому пометки пусты, а `module_init` запоминает функцию для `lx_module_init` — того самого
 * ручного подъёма, которым обёртка заменяет загрузчик модулей ядра.
 */
#ifndef _LINUX_INIT_H_SHIM
#define _LINUX_INIT_H_SHIM

#define __init
#define __exit
#define __initdata
#define __exitdata
#define __devinit
#define __devexit
#define __refdata

#endif /* _LINUX_INIT_H_SHIM */
