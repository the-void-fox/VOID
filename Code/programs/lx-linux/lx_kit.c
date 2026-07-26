/* lx_kit.c — Lx_kit: C-рантайм ядрового API VOID (Веха 56), НЕ исходник Linux.
 *
 * Фундамент под хостинг НЕИЗМЕНЁННЫХ .c ядра Linux: реализует то, что объявляют наши шимы
 * заголовки linux/… , поверх примитивов VOID. Начинаем с самого нужного — аллокатора памяти (семейство
 * kmalloc над кучей newlib: malloc/free → _sbrk → один SYS_MAP) и printk (vprintf → stdout VOID).
 * Здесь же — kmemdup/kstrdup/kstrndup. Растёт к полноценному Lx_kit (slab-кэши, таймеры,
 * workqueue, ioremap/dma) под реальный драйвер подсистемы (e1000 и далее).
 *
 * Флаги gfp_t игнорируются: куча у нас единая, контекст исполнения один.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <linux/printk.h>
#include <linux/slab.h>

void *kmalloc(size_t size, gfp_t flags)
{
	(void)flags;
	return malloc(size ? size : 1); /* kmalloc(0) в ядре не NULL — держим ту же привычку */
}

void *kzalloc(size_t size, gfp_t flags)
{
	(void)flags;
	return calloc(1, size ? size : 1);
}

void *kcalloc(size_t n, size_t size, gfp_t flags)
{
	(void)flags;
	return calloc(n ? n : 1, size ? size : 1);
}

void *kmalloc_array(size_t n, size_t size, gfp_t flags)
{
	if (size != 0 && n > KMALLOC_MAX_SIZE / size) /* защита от переполнения n*size, как в ядре */
		return NULL;
	return kmalloc(n * size, flags);
}

void *krealloc(void *p, size_t new_size, gfp_t flags)
{
	(void)flags;
	if (new_size == 0) {
		free(p);
		return NULL;
	}
	return realloc(p, new_size);
}

void kfree(const void *p)
{
	free((void *)p); /* kfree() в ядре берёт const void * — снимаем const для free() */
}

void *kmemdup(const void *src, size_t len, gfp_t flags)
{
	void *p = kmalloc(len, flags);
	if (p)
		memcpy(p, src, len);
	return p;
}

char *kstrdup(const char *s, gfp_t flags)
{
	size_t len;
	char *p;

	if (!s)
		return NULL;
	len = strlen(s) + 1;
	p = kmalloc(len, flags);
	if (p)
		memcpy(p, s, len);
	return p;
}

char *kstrndup(const char *s, size_t max, gfp_t flags)
{
	size_t len;
	char *p;

	if (!s)
		return NULL;
	len = strnlen(s, max);
	p = kmalloc(len + 1, flags);
	if (p) {
		memcpy(p, s, len);
		p[len] = '\0';
	}
	return p;
}

int printk(const char *fmt, ...)
{
	va_list ap;
	int n;

	va_start(ap, fmt);
	n = vprintf(fmt, ap); /* уровни KERN_* у нас пустые — печатаем строку как есть */
	va_end(ap);
	return n;
}
