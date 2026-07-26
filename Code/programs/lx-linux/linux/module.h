/* linux/module.h — ШИМ lx_emul (Веха 66), НЕ исходник Linux.
 *
 * У нас драйвер не «загружаемый модуль», а часть одного статического ELF. `module_init(fn)`/
 * `module_exit(fn)` перенаправляются в функции с ФИКСИРОВАННЫМИ именами `lx_module_init`/
 * `lx_module_exit` (в бинаре ровно один драйвер = один module_init) — их зовёт наша glue-точка
 * входа. Приём из Genode dde_linux. Остальные MODULE_… и module_param — в пустоту (метаданные
 * модуля нам не нужны, параметры остаются обычными глобалами со значениями по умолчанию).
 */
#ifndef _LINUX_MODULE_H_SHIM
#define _LINUX_MODULE_H_SHIM

#include <linux/export.h>
#include <linux/types.h>

struct module;
#define THIS_MODULE ((struct module *)0)

/* Атрибуты секций инициализации — у нас пустые (всё в .text одного ELF). */
#ifndef __init
#define __init
#endif
#ifndef __exit
#define __exit
#endif
#ifndef __initdata
#define __initdata
#endif
#ifndef __exit_p
#define __exit_p(x) x
#endif
#ifndef __devinit
#define __devinit
#endif

/* Ровно один драйвер на бинарь → фиксированные имена точек входа, их зовёт glue. */
int  lx_module_init(void);
void lx_module_exit(void);
#define module_init(fn) int  lx_module_init(void) { return fn(); }
#define module_exit(fn) void lx_module_exit(void) { fn(); }

/* Метаданные модуля — в пустоту. */
#define MODULE_LICENSE(x)
#define MODULE_AUTHOR(x)
#define MODULE_DESCRIPTION(x)
#define MODULE_VERSION(x)
#define MODULE_INFO(tag, info)
#define MODULE_ALIAS(x)
#define MODULE_FIRMWARE(x)
#define MODULE_DEVICE_TABLE(type, name)
#define MODULE_SOFTDEP(x)

/* Параметры модуля — в пустоту (объявленная переменная остаётся глобалом с дефолтом). */
#define module_param(name, type, perm)
#define module_param_named(alias, name, type, perm)
#define module_param_array(name, type, nump, perm)
#define module_param_string(name, str, len, perm)
#define MODULE_PARM_DESC(name, desc)

#endif /* _LINUX_MODULE_H_SHIM */
