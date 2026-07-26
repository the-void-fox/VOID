/* linux/slab.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * Аллокатор памяти ядра. Реализация — в Lx_kit (lx_kit.c) поверх кучи newlib (_sbrk → один
 * SYS_MAP): семейство kmalloc/kzalloc/kcalloc/kmalloc_array/krealloc/kfree + дубликаторы
 * kmemdup/kstrdup/kstrndup. Флаги gfp_t игнорируются (единая куча, один контекст). Здесь —
 * объявления; тела в Lx_kit-рантайме. Позже сюда придут slab-кэши (kmem_cache_*).
 */
#ifndef _LINUX_SLAB_H_SHIM
#define _LINUX_SLAB_H_SHIM

#include <linux/types.h>
#include <linux/gfp.h>

/* Верхняя граница одного kmalloc (в ядре зависит от страниц/порядка). У нас — разумный потолок,
 * против которого kmalloc_array ловит переполнение n*size. */
#define KMALLOC_MAX_SIZE ((size_t)1 << 25) /* 32 MiB */

void *kmalloc(size_t size, gfp_t flags);
void *kzalloc(size_t size, gfp_t flags);
void *kmalloc_array(size_t n, size_t size, gfp_t flags);
void *kcalloc(size_t n, size_t size, gfp_t flags);
void *krealloc(void *p, size_t new_size, gfp_t flags);
void  kfree(const void *p);

void *kmemdup(const void *src, size_t len, gfp_t flags);
char *kstrdup(const char *s, gfp_t flags);
char *kstrndup(const char *s, size_t max, gfp_t flags);

#endif /* _LINUX_SLAB_H_SHIM */
