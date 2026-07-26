/* lx_sched.h — кооперативный планировщик Lx_kit (Веха 62), API рантайма (НЕ шим Linux).
 *
 * Модель Genode dde_linux: весь портируемый код ядра Linux исполняется в ОДНОМ потоке,
 * задачи переключаются только в явных точках (yield/block). Задача = отдельный стек +
 * setjmp/longjmp. Никакого вытеснения → внутри Linux-кода нет гонок, spinlock/mutex/атомики
 * почти no-op. Поверх этого позже лягут jiffies/таймеры, wait_event/wake_up, workqueue,
 * request_irq и kthread. См. reference/dde-linux/10-lx-kit-runtime.md.
 */
#ifndef LX_SCHED_H
#define LX_SCHED_H

#include <setjmp.h>

enum lx_task_state {
	LX_INIT,     /* создана, ещё ни разу не запускалась (стек не развёрнут) */
	LX_RUNNABLE, /* готова возобновиться */
	LX_RUNNING,  /* исполняется прямо сейчас */
	LX_BLOCKED,  /* ждёт события (unblock/таймер/IRQ) */
	LX_DEAD      /* функция задачи вернулась — под реап */
};

enum lx_task_type {
	LX_TASK_NORMAL, /* обычная ядровая нить/worker */
	LX_TASK_IRQ,    /* обработчик прерывания (будится по IRQ) */
	LX_TASK_TIME    /* таймерный обработчик (будится по jiffies) */
};

struct lx_task {
	jmp_buf              env;       /* точка возобновления самой задачи */
	jmp_buf              saved_env; /* возврат в планировщик (ставится при каждом запуске) */
	void               (*func)(void *);
	void                *arg;
	void                *stack;     /* выделенный блок стека (низ, под free) */
	void                *stack_top; /* вершина, 16-выровнена (стек растёт вниз) */
	enum lx_task_state   state;
	enum lx_task_type    type;
	const char          *name;
	struct lx_task      *next;      /* односвязный список планировщика */
};

/* Завести задачу (добавляется в хвост очереди, стартует со следующего прохода планировщика). */
struct lx_task *lx_task_create(void (*fn)(void *), void *arg, const char *name);
struct lx_task *lx_task_create_typed(void (*fn)(void *), void *arg, const char *name,
                                     enum lx_task_type type);

/* Цикл планировщика: крутит готовые задачи, пока такие есть; возврат — когда все
 * заблокированы или завершились (у Genode тут возврат в EP до внешнего сигнала). */
void lx_sched_run(void);

/* Из контекста задачи: уступить, оставшись готовой (кооперативная точка переключения). */
void lx_sched_yield(void);

/* Из контекста задачи: заблокировать себя (ждать unblock). Модель wait_event. */
void lx_task_block(void);

/* Разблокировать задачу (из другой задачи/по событию). Модель wake_up. No-op, если не BLOCKED. */
void lx_task_unblock(struct lx_task *t);

/* Разбудить все задачи данного типа (под доставку IRQ/таймера позже). */
void lx_sched_wake_type(enum lx_task_type type);

/* Текущая исполняемая задача (NULL вне задачи). */
struct lx_task *lx_task_self(void);

#endif /* LX_SCHED_H */
