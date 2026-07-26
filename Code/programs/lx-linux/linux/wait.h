/* linux/wait.h — ШИМ lx_emul (Веха 64), НЕ исходник Linux.
 *
 * Очереди ожидания ядра: `wait_event(wq, cond)` блокирует задачу, пока условие ложно;
 * `wake_up(wq)` будит ждущих (перепроверят условие). Драйвер так ждёт события железа
 * (сброс/линк/завершение DMA). У нас — поверх block/unblock кооперативного планировщика
 * (Веха 62): очередь держит стек-локальные записи ждущих задач; wake_up переводит их в
 * готовые, каждая по пробуждении сама перепроверяет условие. Таймаут-варианты — через
 * таймер (Веха 63), встроенный в запись ожидания.
 */
#ifndef _LINUX_WAIT_H_SHIM
#define _LINUX_WAIT_H_SHIM

#include <linux/jiffies.h>
#include <linux/timer.h>

struct lx_task;

/* Запись ждущего — живёт на стеке задачи (как wait_queue_entry в Linux' ___wait_event). */
struct lx_wait_entry {
	struct lx_task       *task;
	struct lx_wait_entry *next;
	struct timer_list     timer;     /* для таймаут-ожиданий */
	int                   timed_out; /* выставляет коллбэк таймера */
};

typedef struct wait_queue_head {
	struct lx_wait_entry *waiters;
} wait_queue_head_t;

/* Заблокировать текущую задачу на wq (has_deadline ⇒ ещё и таймер-будильник на deadline). */
void __lx_wait(wait_queue_head_t *wq, struct lx_wait_entry *e, int has_deadline,
               unsigned long deadline);
/* Разбудить всех ждущих на wq (перепроверят условие). */
void __lx_wake_up(wait_queue_head_t *wq);

static inline void init_waitqueue_head(wait_queue_head_t *wq)
{
	wq->waiters = NULL;
}

#define __WAIT_QUEUE_HEAD_INITIALIZER(name) { NULL }
#define DECLARE_WAIT_QUEUE_HEAD(name) wait_queue_head_t name = __WAIT_QUEUE_HEAD_INITIALIZER(name)

/* Будим всегда всех — в один-поток-модели «interruptible/nr»-нюансы не важны. */
#define wake_up(wq)                       __lx_wake_up(wq)
#define wake_up_all(wq)                   __lx_wake_up(wq)
#define wake_up_interruptible(wq)         __lx_wake_up(wq)
#define wake_up_interruptible_all(wq)     __lx_wake_up(wq)

/* Блокироваться, пока condition не станет истинным. */
#define wait_event(wq, condition)                         \
	do {                                              \
		struct lx_wait_entry __e;                 \
		(void)&__e;                               \
		while (!(condition))                      \
			__lx_wait(&(wq), &__e, 0, 0);     \
	} while (0)

/* Сигналов у нас нет — interruptible тождественен и всегда «не прерван» (0). */
#define wait_event_interruptible(wq, condition) \
	({ wait_event(wq, condition); 0; })

/* Вернуть остаток jiffies (>0), если condition наступило; 0 — если истёк timeout. */
#define wait_event_timeout(wq, condition, timeout)                              \
	({                                                                      \
		struct lx_wait_entry __e;                                       \
		unsigned long __deadline = jiffies + (timeout);                 \
		long __ret;                                                     \
		(void)&__e;                                                     \
		for (;;) {                                                      \
			if (condition) {                                        \
				__ret = (long)(__deadline - jiffies);           \
				if (__ret < 1)                                  \
					__ret = 1;                              \
				break;                                          \
			}                                                       \
			if (time_after_eq(jiffies, __deadline)) {               \
				__ret = (condition) ? 1 : 0;                    \
				break;                                          \
			}                                                       \
			__lx_wait(&(wq), &__e, 1, __deadline);                  \
		}                                                               \
		__ret;                                                          \
	})

#define wait_event_interruptible_timeout(wq, condition, timeout) \
	wait_event_timeout(wq, condition, timeout)

#endif /* _LINUX_WAIT_H_SHIM */
