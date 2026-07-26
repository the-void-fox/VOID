/* linux/completion.h — ШИМ lx_emul (Веха 64), НЕ исходник Linux.
 *
 * «Завершение»: одна задача ждёт `wait_for_completion(c)`, другая сигналит `complete(c)`. Тонкая
 * надстройка над очередью ожидания (<linux/wait.h>) со счётчиком `done`. Драйверы ждут так
 * окончания асинхронной операции (проба/сброс/firmware/DMA). Один поток → без гонок счётчика.
 */
#ifndef _LINUX_COMPLETION_H_SHIM
#define _LINUX_COMPLETION_H_SHIM

#include <limits.h> /* UINT_MAX */

#include <linux/jiffies.h>
#include <linux/wait.h>

struct completion {
	unsigned int      done;
	wait_queue_head_t wait;
};

#define COMPLETION_INITIALIZER(name) { 0, __WAIT_QUEUE_HEAD_INITIALIZER((name).wait) }
#define DECLARE_COMPLETION(name) struct completion name = COMPLETION_INITIALIZER(name)

static inline void init_completion(struct completion *c)
{
	c->done = 0;
	init_waitqueue_head(&c->wait);
}

static inline void reinit_completion(struct completion *c)
{
	c->done = 0;
}

/* Сигнал: +1 к счётчику и разбудить ждущих. */
static inline void complete(struct completion *c)
{
	c->done++;
	wake_up(&c->wait);
}

/* Сигнал «навсегда»: любое число ожиданий пройдёт (счётчик не убывает). */
static inline void complete_all(struct completion *c)
{
	c->done = UINT_MAX;
	wake_up(&c->wait);
}

static inline void wait_for_completion(struct completion *c)
{
	wait_event(c->wait, c->done > 0);
	if (c->done != UINT_MAX)
		c->done--;
}

/* Остаток jiffies (>0) при успехе; 0 — если истёк timeout. */
static inline unsigned long wait_for_completion_timeout(struct completion *c, unsigned long timeout)
{
	long r = wait_event_timeout(c->wait, c->done > 0, timeout);

	if (r > 0 && c->done != UINT_MAX)
		c->done--;
	return r > 0 ? (unsigned long)r : 0;
}

#endif /* _LINUX_COMPLETION_H_SHIM */
