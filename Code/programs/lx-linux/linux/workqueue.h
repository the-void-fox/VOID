/* linux/workqueue.h — ШИМ lx_emul (Веха 65), НЕ исходник Linux.
 *
 * Рабочие очереди ядра: отложить функцию `work->func(work)` на потом (`schedule_work`) или на
 * потом-через-задержку (`schedule_delayed_work`). Драйверы так выносят «тяжёлое» из IRQ/таймера
 * в контекст, где МОЖНО спать (у e1000 6.18 watchdog — именно delayed_work). У нас каждую очередь
 * обслуживает выделенная задача-воркер (Lx_kit, Веха 62): `queue_work` кладёт работу и будит
 * воркера; delayed — через таймер (Веха 63), по срабатыванию кладёт работу. Один поток → воркер
 * и заказчик не пересекаются, «sync»-отмена тривиальна.
 */
#ifndef _LINUX_WORKQUEUE_H_SHIM
#define _LINUX_WORKQUEUE_H_SHIM

#include <linux/container_of.h>
#include <linux/jiffies.h>
#include <linux/timer.h>
#include <linux/types.h>

struct workqueue_struct; /* непрозрачна — тело в lx_kit.c */

struct work_struct {
	void (*func)(struct work_struct *);
	/* приватная линковка Lx_kit */
	struct work_struct     *lx_next;
	int                     lx_pending;
	int                     lx_running;
	struct workqueue_struct *lx_wq;
};

struct delayed_work {
	struct work_struct       work;
	struct timer_list        timer;
	struct workqueue_struct *lx_wq;
};

#define to_delayed_work(_w) container_of(_w, struct delayed_work, work)

/* Инициализация таймера delayed_work нашим внутренним коллбэком (тело в lx_kit.c). */
void __lx_init_delayed_timer(struct delayed_work *dw);

#define INIT_WORK(_w, _f)                          \
	do {                                       \
		struct work_struct *__w = (_w);    \
		__w->func = (_f);                  \
		__w->lx_next = NULL;               \
		__w->lx_pending = 0;               \
		__w->lx_running = 0;               \
		__w->lx_wq = NULL;                 \
	} while (0)

#define INIT_WORK_ONSTACK(_w, _f) INIT_WORK(_w, _f)

#define INIT_DELAYED_WORK(_dw, _f)                 \
	do {                                       \
		struct delayed_work *__dw = (_dw); \
		INIT_WORK(&__dw->work, (_f));      \
		__lx_init_delayed_timer(__dw);     \
		__dw->lx_wq = NULL;                \
	} while (0)

#define work_pending(_w)         ((_w)->lx_pending)
#define delayed_work_pending(_dw) (timer_pending(&(_dw)->timer) || (_dw)->work.lx_pending)

/* Системная очередь: у нас — ленивая (функция-геттер). Драйвер лишь ЧИТАЕТ символ. */
struct workqueue_struct *lx_get_system_wq(void);
#define system_wq                  lx_get_system_wq()
#define system_long_wq             lx_get_system_wq()
#define system_unbound_wq          lx_get_system_wq()
#define system_power_efficient_wq  lx_get_system_wq()

bool queue_work(struct workqueue_struct *wq, struct work_struct *w);
bool queue_delayed_work(struct workqueue_struct *wq, struct delayed_work *dw, unsigned long delay);
bool mod_delayed_work(struct workqueue_struct *wq, struct delayed_work *dw, unsigned long delay);

static inline bool schedule_work(struct work_struct *w)
{
	return queue_work(system_wq, w);
}

static inline bool schedule_delayed_work(struct delayed_work *dw, unsigned long delay)
{
	return queue_delayed_work(system_wq, dw, delay);
}

void flush_work(struct work_struct *w);
void flush_delayed_work(struct delayed_work *dw);
void flush_workqueue(struct workqueue_struct *wq);
void flush_scheduled_work(void);

bool cancel_work_sync(struct work_struct *w);
bool cancel_delayed_work(struct delayed_work *dw);
bool cancel_delayed_work_sync(struct delayed_work *dw);

struct workqueue_struct *lx_alloc_workqueue(const char *name);
#define alloc_workqueue(fmt, flags, max_active, ...) lx_alloc_workqueue(fmt)
#define alloc_ordered_workqueue(fmt, flags, ...)     lx_alloc_workqueue(fmt)
#define create_singlethread_workqueue(name)          lx_alloc_workqueue(name)
#define create_workqueue(name)                       lx_alloc_workqueue(name)
void destroy_workqueue(struct workqueue_struct *wq);

#endif /* _LINUX_WORKQUEUE_H_SHIM */
