/* lx_kit.c — Lx_kit: C-рантайм ядрового API VOID (Веха 56), НЕ исходник Linux.
 *
 * Фундамент под хостинг НЕИЗМЕНЁННЫХ .c ядра Linux: реализует то, что объявляют наши шимы
 * заголовки linux/… , поверх примитивов VOID. Начинаем с самого нужного — аллокатора памяти (семейство
 * kmalloc над кучей newlib: malloc/free → _sbrk → один SYS_MAP) и printk (vprintf → stdout VOID).
 * Здесь же — kmemdup/kstrdup/kstrndup. С Вехи 62 — кооперативный планировщик (задача = отдельный
 * стек + setjmp/longjmp; см. lx_sched.h). Растёт к полному Lx_kit (jiffies/таймеры, wait_event/
 * wake_up, workqueue, request_irq, ioremap/dma) под реальный драйвер подсистемы (e1000 и далее).
 *
 * Флаги gfp_t игнорируются: куча у нас единая, контекст исполнения один.
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h> /* gettimeofday — монотонное время VOID под udelay/mdelay */

#include <linux/delay.h>
#include <linux/printk.h>
#include <linux/slab.h>

#include "lx_sched.h" /* кооперативный планировщик (Веха 62) */

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

/* ─── задержки (linux/delay.h) ────────────────────────────────────────────────
 * Буси-ожидание по МОНОТОННОМУ времени VOID (gettimeofday → vsys_ticks, 1–100 нс/тик).
 * udelay/ndelay/mdelay в ядре зовутся и в атомарном контексте — крутимся, не спим. */
static unsigned long long now_us(void)
{
	struct timeval tv;
	gettimeofday(&tv, NULL);
	return (unsigned long long)tv.tv_sec * 1000000ull + (unsigned long long)tv.tv_usec;
}

void udelay(unsigned long usecs)
{
	unsigned long long start = now_us();
	while (now_us() - start < usecs)
		; /* буси-ожидание */
}

void ndelay(unsigned long nsecs)
{
	udelay((nsecs + 999) / 1000); /* разрешение времени — микросекунда; округляем вверх */
}

void mdelay(unsigned long msecs)
{
	while (msecs--)
		udelay(1000);
}

void msleep(unsigned int msecs)
{
	mdelay(msecs); /* пока тоже буси; уступающий сон — с планировщиком Lx_kit позже */
}

/* ─── кооперативный планировщик (lx_sched.h, Веха 62) ─────────────────────────
 * Один поток, задача = отдельный стек + setjmp/longjmp. Планировщик и задача
 * ping-понгуют управление: задача уступает через longjmp в saved_env (возврат в
 * планировщик), планировщик возобновляет через longjmp в env (в точку yield/block).
 * Первый запуск задачи — arch_execute: смена SP на её стек + вызов трамплина.
 * Модель Genode dde_linux (task.cc/scheduler.cc). */

#define LX_STACK_SIZE (32 * 1024) /* как у Genode: хватает newlib-printf с запасом */

static struct lx_task *sched_head;    /* голова очереди задач */
static struct lx_task *sched_tail;    /* хвост (для O(1) append и move-to-tail) */
static struct lx_task *sched_current; /* текущая исполняемая задача */

static void list_append(struct lx_task *t)
{
	t->next = NULL;
	if (sched_tail)
		sched_tail->next = t;
	else
		sched_head = t;
	sched_tail = t;
}

static void list_remove(struct lx_task *t)
{
	struct lx_task *prev = NULL, *c = sched_head;

	while (c) {
		if (c == t) {
			if (prev)
				prev->next = c->next;
			else
				sched_head = c->next;
			if (sched_tail == t)
				sched_tail = prev;
			c->next = NULL;
			return;
		}
		prev = c;
		c = c->next;
	}
}

/* Переключить стек на `sp` и вызвать fn() — обратно не возвращается: задача уходит
 * из планировщика/умирает через longjmp. Короткая арх-вставка (riscv64 + x86_64). */
__attribute__((noreturn))
static void arch_execute(void *sp, void (*fn)(void))
{
#if defined(__riscv)
	__asm__ volatile("mv sp, %0\n\t"
	                 "jalr %1\n\t"
	                 : : "r"(sp), "r"(fn) : "memory");
#elif defined(__x86_64__)
	__asm__ volatile("movq %0, %%rsp\n\t"
	                 "call *%1\n\t"
	                 : : "r"(sp), "r"(fn) : "memory");
#else
#error "arch_execute: неизвестная архитектура (нужен riscv64 или x86_64)"
#endif
	__builtin_unreachable();
}

/* Тело задачи на её собственном стеке: отработать func и умереть (назад — в планировщик). */
static void lx_task_trampoline(void)
{
	struct lx_task *t = sched_current;

	t->func(t->arg);
	t->state = LX_DEAD;
	longjmp(t->saved_env, 1);
}

/* Запустить/возобновить задачу; вернётся, когда задача уступит, заблокируется или умрёт. */
static void lx_task_run(struct lx_task *t)
{
	if (setjmp(t->saved_env)) /* сюда возвращают yield/block/смерть задачи */
		return;

	if (t->state == LX_INIT) {
		t->state = LX_RUNNING;
		arch_execute(t->stack_top, lx_task_trampoline); /* первый запуск — на свой стек */
	} else {
		t->state = LX_RUNNING;
		longjmp(t->env, 1); /* возобновление с точки yield/block */
	}
}

struct lx_task *lx_task_create_typed(void (*fn)(void *), void *arg, const char *name,
                                     enum lx_task_type type)
{
	struct lx_task *t = calloc(1, sizeof(*t));
	uintptr_t top;

	if (!t)
		return NULL;
	t->stack = malloc(LX_STACK_SIZE);
	if (!t->stack) {
		free(t);
		return NULL;
	}
	top = ((uintptr_t)t->stack + LX_STACK_SIZE) & ~(uintptr_t)15; /* вершина, 16-выровнена */
	t->stack_top = (void *)top;
	t->func = fn;
	t->arg = arg;
	t->name = name;
	t->state = LX_INIT;
	t->type = type;
	list_append(t);
	return t;
}

struct lx_task *lx_task_create(void (*fn)(void *), void *arg, const char *name)
{
	return lx_task_create_typed(fn, arg, name, LX_TASK_NORMAL);
}

void lx_sched_yield(void)
{
	if (setjmp(sched_current->env)) /* возобновлены — просто вернуться в задачу */
		return;
	sched_current->state = LX_RUNNABLE;
	/* в хвост очереди — честный round-robin среди равных готовых задач */
	list_remove(sched_current);
	list_append(sched_current);
	longjmp(sched_current->saved_env, 1);
}

void lx_task_block(void)
{
	if (setjmp(sched_current->env))
		return;
	sched_current->state = LX_BLOCKED;
	longjmp(sched_current->saved_env, 1);
}

void lx_task_unblock(struct lx_task *t)
{
	if (t && t->state == LX_BLOCKED)
		t->state = LX_RUNNABLE;
}

void lx_sched_wake_type(enum lx_task_type type)
{
	struct lx_task *t;

	for (t = sched_head; t; t = t->next)
		if (t->type == type && t->state == LX_BLOCKED)
			t->state = LX_RUNNABLE;
}

struct lx_task *lx_task_self(void)
{
	return sched_current;
}

void lx_sched_run(void)
{
	struct lx_task *t, *next;

	for (;;) {
		t = sched_head;
		while (t && t->state != LX_RUNNABLE && t->state != LX_INIT)
			t = t->next; /* первая готовая от головы (порядок = приоритет) */
		if (!t)
			break;       /* готовых нет — все блокированы/мертвы */
		sched_current = t;
		lx_task_run(t);
		sched_current = NULL;
	}

	/* Реап завершённых — после остановки цикла: указатели на задачи не должны
	 * висеть, пока другая задача ещё может к ним обратиться (unblock и т.п.).
	 * Долгоживущий планировщик драйвера будет реапить инкрементно — потом. */
	for (t = sched_head; t; t = next) {
		next = t->next;
		if (t->state == LX_DEAD) {
			list_remove(t);
			free(t->stack);
			free(t);
		}
	}
}
