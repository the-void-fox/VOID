/* linux/uaccess.h — ШИМ lx_emul (Веха 193), НЕ исходник Linux.
 *
 * Граница «ядро ↔ пользователь», которой у нас НЕТ: драйвер целиком живёт в одном процессе
 * userspace VOID, и указатель, помеченный `__user`, указывает в ту же память, что и обычный.
 * Поэтому копирование — это memcpy, а проверка доступа — правда.
 *
 * Важно понимать, чем это НЕ является: послаблением безопасности. Настоящая граница в VOID
 * проходит не здесь, а по capability — драйвер не может тронуть чужую память не потому, что
 * `copy_from_user` проверяет указатель, а потому, что ему не выдано права на неё.
 */
#ifndef _LINUX_UACCESS_H_SHIM
#define _LINUX_UACCESS_H_SHIM

#include <linux/types.h>
#include <linux/string.h>

#define __user

static inline unsigned long copy_to_user(void *to, const void *from, unsigned long n)
{
	memcpy(to, from, n);
	return 0;
}

static inline unsigned long copy_from_user(void *to, const void *from, unsigned long n)
{
	memcpy(to, from, n);
	return 0;
}

static inline int access_ok(const void *addr, unsigned long size)
{
	(void)addr; (void)size;
	return 1;
}

#endif /* _LINUX_UACCESS_H_SHIM */
