/* linux/container_of.h — ШИМ lx_emul (Веха 57), НЕ исходник Linux.
 *
 * container_of() — из указателя на поле структуры получить указатель на саму структуру. Базовая
 * идиома ядра (в настоящем ядре с 6.x вынесена в отдельный <linux/container_of.h>). Отдельным
 * файлом, чтобы <linux/list.h> опирался на неё, не таща весь «зонтик» <linux/kernel.h>.
 */
#ifndef _LINUX_CONTAINER_OF_H_SHIM
#define _LINUX_CONTAINER_OF_H_SHIM

#include <stddef.h> /* offsetof */

#ifndef container_of
#define container_of(ptr, type, member) \
	((type *)((char *)(ptr) - offsetof(type, member)))
#endif

#endif /* _LINUX_CONTAINER_OF_H_SHIM */
