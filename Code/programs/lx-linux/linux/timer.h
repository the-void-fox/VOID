/* linux/timer.h — ШИМ lx_emul (Веха 63), НЕ исходник Linux.
 *
 * Таймеры ядра: одноразовый коллбэк `function(timer)` в момент `expires` (в jiffies). Substrate под
 * watchdog'и драйверов, delayed_work/workqueue и таймауты. У нас очередь таймеров живёт в Lx_kit
 * (lx_kit.c): цикл планировщика в idle-пути двигает jiffies по реальному времени и стреляет
 * выстрелившими таймерами (softirq-контекст: `lx_task_self()` там NULL, как в Linux). Модель Genode:
 * коллбэк исполняется в том же ОДНОМ потоке, что и весь ядровый код.
 *
 * Наша `struct timer_list` — шимовая: драйвер трогает её только через API + поля expires/function и
 * `timer_container_of`/`from_timer`; линковка очереди — в приватных полях lx_*.
 */
#ifndef _LINUX_TIMER_H_SHIM
#define _LINUX_TIMER_H_SHIM

#include <linux/container_of.h>
#include <linux/jiffies.h>
#include <linux/types.h>

struct timer_list {
	unsigned long expires;                     /* когда стрелять, в jiffies */
	void        (*function)(struct timer_list *);
	u32           flags;
	/* приватная линковка Lx_kit (в настоящем ядре — hlist в timer-колесе) */
	struct timer_list *lx_next;
	int                lx_pending;
};

#define TIMER_IRQSAFE 0x00200000u /* флаги ядра игнорируем (один поток) — держим имя для совместимости */

void __lx_timer_setup(struct timer_list *t, void (*fn)(struct timer_list *), unsigned int flags);
/* Инициализация таймера (ядро 4.15+: коллбэк берёт struct timer_list *). */
#define timer_setup(timer, callback, flags) __lx_timer_setup((timer), (callback), (flags))

int  mod_timer(struct timer_list *t, unsigned long expires);
void add_timer(struct timer_list *t);
int  timer_delete(struct timer_list *t);
int  timer_delete_sync(struct timer_list *t);
int  timer_pending(const struct timer_list *t);

/* Легаси-имена (в 6.x переименованы, но встречаются): у нас — тождественно (один поток). */
#define del_timer(t)      timer_delete(t)
#define del_timer_sync(t) timer_delete_sync(t)

/* Из указателя на встроенный timer_list — указатель на объемлющую структуру драйвера. */
#define timer_container_of(var, callback_timer, timer_fieldname) \
	container_of(callback_timer, typeof(*(var)), timer_fieldname)
/* Легаси-имя той же операции. */
#define from_timer(var, callback_timer, timer_fieldname) \
	timer_container_of(var, callback_timer, timer_fieldname)

#endif /* _LINUX_TIMER_H_SHIM */
