/* linux/string.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * Работа со строками/памятью. Функции mem… и str… берём готовыми из newlib (<string.h>) — те же
 * имена и семантика, что ждёт код ядра (memcpy/memset/memmove/strlen/strnlen/strcmp/…). Плюс объявления
 * argv_split()/argv_free(): в настоящем ядре они живут именно в <linux/string.h>. Дубликаторы с
 * аллокацией (kstrdup/kmemdup/kstrndup) — в <linux/slab.h>. Растёт по мере надобности.
 */
#ifndef _LINUX_STRING_H_SHIM
#define _LINUX_STRING_H_SHIM

#include <string.h>      /* newlib: memcpy/memset/memmove/strlen/strnlen/strcmp/strncmp/… */
#include <linux/gfp.h>   /* gfp_t для сигнатуры argv_split() */

char **argv_split(gfp_t gfp, const char *str, int *argcp);
void argv_free(char **argv);

/* strscpy — безопасное копирование строки (ядро 4.3+): не более size, всегда \0-терминирует;
 * возвращает длину скопированного или -E2BIG при усечении. Тело — в lx_kit.c. */
ssize_t strscpy(char *dst, const char *src, size_t size);

#endif /* _LINUX_STRING_H_SHIM */
