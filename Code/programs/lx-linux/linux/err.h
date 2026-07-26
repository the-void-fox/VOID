/* linux/err.h — ШИМ lx_emul (Веха 59), НЕ исходник Linux.
 *
 * Идиома ядра «ошибка в указателе»: маленькие отрицательные значения (−1..−4095, коды errno)
 * кодируются как невалидные указатели в верхней странице адресов, так функции возвращают ЛИБО
 * настоящий указатель, ЛИБО ERR_PTR(-Exxx) без отдельного out-параметра. Даём ровно ядровый набор
 * (ERR_PTR/PTR_ERR/IS_ERR/IS_ERR_OR_NULL/PTR_ERR_OR_ZERO/ERR_CAST) — тем же приёмом.
 */
#ifndef _LINUX_ERR_H_SHIM
#define _LINUX_ERR_H_SHIM

#include <linux/errno.h>
#include <linux/types.h>

#define MAX_ERRNO 4095

/* Значение — код ошибки (верхняя страница адресов недоступна пользователю). */
#define IS_ERR_VALUE(x) ((unsigned long)(void *)(x) >= (unsigned long)-MAX_ERRNO)

static inline void *ERR_PTR(long error)
{
	return (void *)error;
}
static inline long PTR_ERR(const void *ptr)
{
	return (long)ptr;
}
static inline bool IS_ERR(const void *ptr)
{
	return IS_ERR_VALUE((unsigned long)ptr);
}
static inline bool IS_ERR_OR_NULL(const void *ptr)
{
	return !ptr || IS_ERR_VALUE((unsigned long)ptr);
}
/* Перенести код ошибки на указатель другого типа (без предупреждений). */
static inline void *ERR_CAST(const void *ptr)
{
	return (void *)ptr;
}
static inline long PTR_ERR_OR_ZERO(const void *ptr)
{
	return IS_ERR(ptr) ? PTR_ERR(ptr) : 0;
}

#endif /* _LINUX_ERR_H_SHIM */
