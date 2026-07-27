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
#include <linux/device.h>
#include <linux/jiffies.h>
#include <linux/pci.h>
#include <linux/printk.h>
#include <linux/slab.h>
#include <linux/timer.h>
#include <linux/wait.h>
#include <linux/workqueue.h>

#include "lx_sched.h" /* кооперативный планировщик (Веха 62) */

#ifdef LX_HAVE_SYSCALL
#include <syscall.h> /* vsys_irq_wait — доставка IRQ в idle-пути планировщика (Веха 72) */
#endif

static void lx_jiffies_update(void); /* двигает jiffies по монотонному времени VOID (Веха 63) */

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

/* msleep — УСТУПАЮЩИЙ сон (Веха 63). В контексте задачи ставит таймер, который её разбудит, и
 * блокируется (отдаёт процессор другим задачам); вне задачи (нет планировщика) — буси-mdelay. */
struct lx_sleep_timer {
	struct timer_list t;
	struct lx_task   *task;
};

static void lx_sleep_wake(struct timer_list *tl)
{
	struct lx_sleep_timer *st = from_timer(st, tl, t);
	lx_task_unblock(st->task);
}

void msleep(unsigned int msecs)
{
	struct lx_task *self = lx_task_self();
	struct lx_sleep_timer st;

	if (!self) {
		mdelay(msecs); /* вне задачи — честная буси-пауза (как было) */
		return;
	}
	lx_jiffies_update();
	st.task = self;
	__lx_timer_setup(&st.t, lx_sleep_wake, 0);
	mod_timer(&st.t, jiffies + msecs_to_jiffies(msecs ? msecs : 1));
	lx_task_block();     /* уступаем; таймер выстрелит в idle-пути → unblock → вернёмся сюда */
	timer_delete(&st.t); /* снять на всякий случай (если разбудили не таймером) */
}

/* ─── jiffies + таймеры (linux/jiffies.h, linux/timer.h, Веха 63) ─────────────
 * jiffies двигается по МОНОТОННОМУ времени VOID (now_us), обновляется в точках
 * планирования/задержки. Таймеры — односвязная очередь; цикл планировщика в idle-
 * пути стреляет выстрелившими (softirq-контекст: sched_current == NULL). */

unsigned long volatile jiffies;            /* глобальный счётчик тиков (linux/jiffies.h) */
static unsigned long long jiffies_boot_us; /* точка отсчёта (0 = ещё не инициализировано) */
static struct timer_list *timer_head;      /* очередь заведённых таймеров */

static void lx_jiffies_update(void)
{
	if (!jiffies_boot_us)
		jiffies_boot_us = now_us(); /* ленивая инициализация точки отсчёта */
	/* 1000000/HZ мкс на один jiffy (HZ=100 → 10000 мкс = 10 мс) */
	jiffies = (unsigned long)((now_us() - jiffies_boot_us) / (1000000ull / HZ));
}

u64 get_jiffies_64(void)
{
	return jiffies;
}

void __lx_timer_setup(struct timer_list *t, void (*fn)(struct timer_list *), unsigned int flags)
{
	t->function = fn;
	t->flags = flags;
	t->expires = 0;
	t->lx_next = NULL;
	t->lx_pending = 0;
}

static void lx_timer_unlink(struct timer_list *t)
{
	struct timer_list *prev = NULL, *c = timer_head;

	while (c) {
		if (c == t) {
			if (prev)
				prev->lx_next = c->lx_next;
			else
				timer_head = c->lx_next;
			break;
		}
		prev = c;
		c = c->lx_next;
	}
	t->lx_next = NULL;
	t->lx_pending = 0;
}

int mod_timer(struct timer_list *t, unsigned long expires)
{
	int was = t->lx_pending;

	if (was)
		lx_timer_unlink(t);
	t->expires = expires;
	t->lx_next = timer_head; /* в голову — порядок в очереди не важен, выбираем по expires */
	timer_head = t;
	t->lx_pending = 1;
	return was;
}

void add_timer(struct timer_list *t)
{
	mod_timer(t, t->expires);
}

int timer_delete(struct timer_list *t)
{
	int was = t->lx_pending;

	if (was)
		lx_timer_unlink(t);
	return was;
}

int timer_delete_sync(struct timer_list *t)
{
	return timer_delete(t); /* один поток — sync-вариант тождествен */
}

int timer_pending(const struct timer_list *t)
{
	return t->lx_pending;
}

/* Выстрелить все таймеры, чей срок наступил (jiffies уже обновлён). Коллбэк может
 * перевзвести/удалить таймеры, поэтому каждый раз ищем due заново от головы. Возвращает
 * число сработавших (>0 ⇒ мог кого-то разблокировать). */
static int lx_timers_fire_due(void)
{
	int fired = 0;

	for (;;) {
		struct timer_list *t = timer_head, *due = NULL;

		while (t) {
			if (time_after_eq(jiffies, t->expires)) {
				due = t;
				break;
			}
			t = t->lx_next;
		}
		if (!due)
			break;
		lx_timer_unlink(due);  /* снять ДО коллбэка: он вправе перевзвести этот же таймер */
		due->function(due);
		fired++;
	}
	return fired;
}

/* Ближайший срок среди заведённых таймеров. 1 + *next, если есть; иначе 0. */
static int lx_timers_next(unsigned long *next_exp)
{
	struct timer_list *t;
	unsigned long min = 0;
	int any = 0;

	for (t = timer_head; t; t = t->lx_next) {
		if (!any || time_before(t->expires, min)) {
			min = t->expires;
			any = 1;
		}
	}
	if (any)
		*next_exp = min;
	return any;
}

/* ─── очереди ожидания (linux/wait.h, Веха 64) ────────────────────────────────
 * wait_event блокирует задачу на wq (запись ждущего — на её стеке), wake_up переводит
 * ждущих в готовые (перепроверят условие сами). Таймаут — через встроенный таймер. */

static void lx_wait_timer_cb(struct timer_list *tl)
{
	struct lx_wait_entry *e = from_timer(e, tl, timer);

	e->timed_out = 1;
	lx_task_unblock(e->task);
}

void __lx_wait(wait_queue_head_t *wq, struct lx_wait_entry *e, int has_deadline,
               unsigned long deadline)
{
	struct lx_wait_entry **pp;

	e->task = lx_task_self();
	e->timed_out = 0;
	e->next = wq->waiters; /* в голову списка ждущих */
	wq->waiters = e;

	if (has_deadline) {
		__lx_timer_setup(&e->timer, lx_wait_timer_cb, 0);
		mod_timer(&e->timer, deadline);
	}

	lx_task_block(); /* уступаем; вернёмся по wake_up или по таймеру */

	if (has_deadline)
		timer_delete(&e->timer);

	for (pp = &wq->waiters; *pp; pp = &(*pp)->next) /* снять свою запись */
		if (*pp == e) {
			*pp = e->next;
			break;
		}
}

void __lx_wake_up(wait_queue_head_t *wq)
{
	struct lx_wait_entry *e;

	for (e = wq->waiters; e; e = e->next)
		lx_task_unblock(e->task);
}

/* ─── рабочие очереди (linux/workqueue.h, Веха 65) ────────────────────────────
 * Каждую очередь обслуживает задача-воркер: крутит очередь работ, блокируется, когда
 * пусто; queue_work кладёт работу и будит воркера. delayed_work — через таймер (Веха 63).
 * flush_* блокирует заказчика на flush-очереди воркера, пока работа не отработает. */

struct workqueue_struct {
	struct work_struct *pending_head;
	struct work_struct *pending_tail;
	struct lx_task     *worker;
	wait_queue_head_t   flush_wq; /* воркер будит после каждой работы — для flush_* */
	const char         *name;
};

static struct workqueue_struct *lx_system_wq; /* ленивая системная очередь */

static void lx_work_unlink(struct workqueue_struct *wq, struct work_struct *w)
{
	struct work_struct **pp = &wq->pending_head, *prev = NULL;

	for (; *pp; prev = *pp, pp = &(*pp)->lx_next)
		if (*pp == w) {
			*pp = w->lx_next;
			if (wq->pending_tail == w)
				wq->pending_tail = prev;
			w->lx_next = NULL;
			w->lx_pending = 0;
			return;
		}
}

static void lx_worker_fn(void *arg)
{
	struct workqueue_struct *wq = arg;

	for (;;) {
		while (wq->pending_head) {
			struct work_struct *w = wq->pending_head;

			wq->pending_head = w->lx_next;
			if (!wq->pending_head)
				wq->pending_tail = NULL;
			w->lx_next = NULL;
			w->lx_pending = 0;
			w->lx_running = 1;
			w->func(w);           /* работа исполняется в контексте задачи — МОЖНО спать */
			w->lx_running = 0;
			__lx_wake_up(&wq->flush_wq); /* разбудить ждущих flush_* */
		}
		lx_task_block(); /* очередь пуста — спим до queue_work */
	}
}

struct workqueue_struct *lx_alloc_workqueue(const char *name)
{
	struct workqueue_struct *wq = calloc(1, sizeof(*wq));

	if (!wq)
		return NULL;
	wq->name = name;
	init_waitqueue_head(&wq->flush_wq);
	wq->worker = lx_task_create(lx_worker_fn, wq, name ? name : "wq"); /* воркер */
	return wq;
}

struct workqueue_struct *lx_get_system_wq(void)
{
	if (!lx_system_wq)
		lx_system_wq = lx_alloc_workqueue("events");
	return lx_system_wq;
}

bool queue_work(struct workqueue_struct *wq, struct work_struct *w)
{
	if (w->lx_pending) /* уже в очереди — ядро не перезаводит */
		return false;
	w->lx_pending = 1;
	w->lx_wq = wq;
	w->lx_next = NULL;
	if (wq->pending_tail)
		wq->pending_tail->lx_next = w;
	else
		wq->pending_head = w;
	wq->pending_tail = w;
	lx_task_unblock(wq->worker); /* разбудить воркера */
	return true;
}

/* Таймер delayed_work: по срабатыванию кладём работу в её очередь. */
static void lx_delayed_work_timer(struct timer_list *tl)
{
	struct delayed_work *dw = from_timer(dw, tl, timer);

	queue_work(dw->lx_wq, &dw->work);
}

void __lx_init_delayed_timer(struct delayed_work *dw)
{
	__lx_timer_setup(&dw->timer, lx_delayed_work_timer, 0);
}

bool queue_delayed_work(struct workqueue_struct *wq, struct delayed_work *dw, unsigned long delay)
{
	dw->lx_wq = wq;
	dw->work.lx_wq = wq;
	if (delay == 0)
		return queue_work(wq, &dw->work); /* без задержки — сразу */
	lx_jiffies_update();
	mod_timer(&dw->timer, jiffies + delay);
	return true;
}

bool mod_delayed_work(struct workqueue_struct *wq, struct delayed_work *dw, unsigned long delay)
{
	timer_delete(&dw->timer);
	lx_work_unlink(wq, &dw->work);
	queue_delayed_work(wq, dw, delay);
	return true;
}

void flush_work(struct work_struct *w)
{
	if (w->lx_wq)
		wait_event(w->lx_wq->flush_wq, !w->lx_pending && !w->lx_running);
}

void flush_delayed_work(struct delayed_work *dw)
{
	if (dw->lx_wq)
		wait_event(dw->lx_wq->flush_wq,
		           !timer_pending(&dw->timer) && !dw->work.lx_pending && !dw->work.lx_running);
}

void flush_workqueue(struct workqueue_struct *wq)
{
	if (wq)
		wait_event(wq->flush_wq, !wq->pending_head);
}

void flush_scheduled_work(void)
{
	flush_workqueue(lx_get_system_wq());
}

bool cancel_work_sync(struct work_struct *w)
{
	bool was = w->lx_pending;

	if (w->lx_wq && w->lx_pending)
		lx_work_unlink(w->lx_wq, w);
	return was; /* один поток: воркер не может исполнять w прямо сейчас — sync тривиален */
}

bool cancel_delayed_work(struct delayed_work *dw)
{
	bool was = timer_pending(&dw->timer) || dw->work.lx_pending;

	timer_delete(&dw->timer);
	if (dw->lx_wq && dw->work.lx_pending)
		lx_work_unlink(dw->lx_wq, &dw->work);
	return was;
}

bool cancel_delayed_work_sync(struct delayed_work *dw)
{
	return cancel_delayed_work(dw);
}

void destroy_workqueue(struct workqueue_struct *wq)
{
	if (wq)
		flush_workqueue(wq); /* воркер (задача) остаётся заблокированным; освободим при реапе */
}

/* ─── driver-model (linux/device.h, Веха 66) ──────────────────────────────────
 * Списки зарегистрированных драйверов и устройств; регистрация связывает их по
 * правилу шины (`bus->match`) и вызывает `.probe`. Упрощённый drivers/base/dd.c. */

static struct device_driver *lx_drivers; /* список драйверов */
static struct device        *lx_devices; /* список устройств */

static int lx_match(struct device *dev, struct device_driver *drv)
{
	if (dev->bus != drv->bus || !dev->bus)
		return 0;
	if (dev->bus->match)
		return dev->bus->match(dev, drv);
	return 0; /* без матчера шины связать не можем */
}

/* Попытка связать устройство с драйвером: match → bind → probe (при провале — отвязать). */
static void lx_try_bind(struct device *dev, struct device_driver *drv)
{
	if (dev->driver)
		return; /* уже связано */
	if (!lx_match(dev, drv))
		return;
	dev->driver = drv;
	if (drv->probe && drv->probe(dev) != 0)
		dev->driver = NULL; /* probe отказал (в т.ч. -EPROBE_DEFER) — откат */
}

int bus_register(struct bus_type *bus)
{
	(void)bus; /* глобального реестра шин не держим — match идёт через dev->bus */
	return 0;
}

void bus_unregister(struct bus_type *bus)
{
	(void)bus;
}

int driver_register(struct device_driver *drv)
{
	struct device *d;

	drv->lx_next = lx_drivers; /* в список драйверов */
	lx_drivers = drv;
	for (d = lx_devices; d; d = d->lx_next) /* попробовать связать с уже известными устройствами */
		lx_try_bind(d, drv);
	return 0;
}

void driver_unregister(struct device_driver *drv)
{
	struct device_driver **pp;
	struct device *d;

	for (d = lx_devices; d; d = d->lx_next) /* отвязать связанные устройства */
		if (d->driver == drv) {
			if (drv->remove)
				drv->remove(d);
			d->driver = NULL;
		}
	for (pp = &lx_drivers; *pp; pp = &(*pp)->lx_next)
		if (*pp == drv) {
			*pp = drv->lx_next;
			break;
		}
}

int device_add(struct device *dev)
{
	struct device_driver *drv;

	dev->lx_next = lx_devices; /* в список устройств */
	lx_devices = dev;
	for (drv = lx_drivers; drv; drv = drv->lx_next) { /* найти драйвер */
		lx_try_bind(dev, drv);
		if (dev->driver)
			break;
	}
	return 0;
}

int device_register(struct device *dev)
{
	return device_add(dev);
}

void device_del(struct device *dev)
{
	struct device **pp;

	if (dev->driver) { /* отвязать от драйвера */
		if (dev->driver->remove)
			dev->driver->remove(dev);
		dev->driver = NULL;
	}
	for (pp = &lx_devices; *pp; pp = &(*pp)->lx_next)
		if (*pp == dev) {
			*pp = dev->lx_next;
			break;
		}
}

void device_unregister(struct device *dev)
{
	device_del(dev);
}

/* ─── PCI поверх driver-model (linux/pci.h, Веха 67) ──────────────────────────
 * `pci_dev` встраивает `struct device`, `pci_driver` — `struct device_driver`.
 * Регистрация переиспользует driver_register/match/probe Вехи 66: match идёт по
 * таблице id_table (vendor/device), а мосты lx_pci_dev_probe/remove разворачивают
 * базовый вызов обратно в pci_driver->probe(pdev, id). Конфиг/BAR — учётные. */

/* Одна запись id_table подходит устройству? PCI_ANY_ID совпадает с любым. */
static int lx_pci_id_match(const struct pci_device_id *id, struct pci_dev *pdev)
{
	if (id->vendor != (u32)PCI_ANY_ID && id->vendor != pdev->vendor)
		return 0;
	if (id->device != (u32)PCI_ANY_ID && id->device != pdev->device)
		return 0;
	if (id->subvendor != (u32)PCI_ANY_ID && id->subvendor != pdev->subsystem_vendor)
		return 0;
	if (id->subdevice != (u32)PCI_ANY_ID && id->subdevice != pdev->subsystem_device)
		return 0;
	return 1;
}

/* match шины PCI: перебрать id_table драйвера; при совпадении запомнить id/драйвер. */
static int lx_pci_bus_match(struct device *dev, struct device_driver *drv)
{
	struct pci_dev *pdev = to_pci_dev(dev);
	struct pci_driver *pdrv = to_pci_driver(drv);
	const struct pci_device_id *id;

	if (!pdrv->id_table)
		return 0;
	for (id = pdrv->id_table; id->vendor || id->device || id->subvendor; id++)
		if (lx_pci_id_match(id, pdev)) {
			pdev->lx_id = id;
			pdev->lx_driver = pdrv;
			return 1;
		}
	return 0;
}

static struct bus_type pci_bus_type = {
	.name  = "pci",
	.match = lx_pci_bus_match,
};

/* Мост probe: базовый device.probe → pci_driver.probe(pdev, совпавший id). */
static int lx_pci_dev_probe(struct device *dev)
{
	struct pci_dev *pdev = to_pci_dev(dev);

	if (pdev->lx_driver && pdev->lx_driver->probe)
		return pdev->lx_driver->probe(pdev, pdev->lx_id);
	return 0;
}

/* Мост remove: базовый device.remove → pci_driver.remove(pdev). */
static void lx_pci_dev_remove(struct device *dev)
{
	struct pci_dev *pdev = to_pci_dev(dev);

	if (pdev->lx_driver && pdev->lx_driver->remove)
		pdev->lx_driver->remove(pdev);
}

int pci_register_driver(struct pci_driver *drv)
{
	drv->driver.name   = drv->name;   /* проецируем на базовый драйвер... */
	drv->driver.bus    = &pci_bus_type;
	drv->driver.probe  = lx_pci_dev_probe;
	drv->driver.remove = lx_pci_dev_remove;
	return driver_register(&drv->driver); /* ...и крутим ту же связку Вехи 66 */
}

void pci_unregister_driver(struct pci_driver *drv)
{
	driver_unregister(&drv->driver);
}

/* Внести синтетическое устройство в шину PCI (роль перечислителя ядра). */
int lx_pci_register_device(struct pci_dev *pdev)
{
	pdev->dev.bus = &pci_bus_type;
	return device_register(&pdev->dev);
}

/* Учётное конфиг-слово COMMAND: собираем/разбираем бит в lx_config[PCI_COMMAND]. */
static void lx_pci_cmd_set(struct pci_dev *pdev, u16 bits)
{
	u16 cmd = (u16)(pdev->lx_config[PCI_COMMAND] | (pdev->lx_config[PCI_COMMAND + 1] << 8));
	cmd |= bits;
	pdev->lx_config[PCI_COMMAND]     = (u8)(cmd & 0xff);
	pdev->lx_config[PCI_COMMAND + 1] = (u8)(cmd >> 8);
}

int pci_enable_device(struct pci_dev *dev)     { lx_pci_cmd_set(dev, PCI_COMMAND_IO | PCI_COMMAND_MEMORY); return 0; }
int pci_enable_device_mem(struct pci_dev *dev) { lx_pci_cmd_set(dev, PCI_COMMAND_MEMORY); return 0; }
void pci_disable_device(struct pci_dev *dev)   { (void)dev; }
void pci_set_master(struct pci_dev *dev)       { lx_pci_cmd_set(dev, PCI_COMMAND_MASTER); }
int  pci_set_mwi(struct pci_dev *dev)          { lx_pci_cmd_set(dev, PCI_COMMAND_INVALIDATE); return 0; }
void pci_clear_mwi(struct pci_dev *dev)        { (void)dev; }

/* Битовая маска BAR'ов, чьи флаги содержат запрошенные (реальная семантика). */
int pci_select_bars(struct pci_dev *dev, unsigned long flags)
{
	int i, bars = 0;

	for (i = 0; i < PCI_STD_NUM_BARS; i++)
		if (dev->resource[i].flags & flags)
			bars |= 1 << i;
	return bars;
}

int pci_request_selected_regions(struct pci_dev *dev, int bars, const char *name)
{
	(void)dev; (void)bars; (void)name; /* менеджера регионов не держим */
	return 0;
}

void pci_release_selected_regions(struct pci_dev *dev, int bars)
{
	(void)dev; (void)bars;
}

/* Окно BAR: ioremap identity над стартом BAR (реальное окно — по MMIO-cap VOID). */
void __iomem *pci_ioremap_bar(struct pci_dev *dev, int bar)
{
	return ioremap(pci_resource_start(dev, bar), pci_resource_len(dev, bar));
}

/* ─ конфиг-пространство: little-endian чтение/запись над lx_config[] ─ */
int pci_read_config_byte(struct pci_dev *dev, int where, u8 *val)
{
	*val = dev->lx_config[where];
	return 0;
}
int pci_read_config_word(struct pci_dev *dev, int where, u16 *val)
{
	*val = (u16)(dev->lx_config[where] | (dev->lx_config[where + 1] << 8));
	return 0;
}
int pci_read_config_dword(struct pci_dev *dev, int where, u32 *val)
{
	*val = (u32)dev->lx_config[where]            | ((u32)dev->lx_config[where + 1] << 8) |
	       ((u32)dev->lx_config[where + 2] << 16) | ((u32)dev->lx_config[where + 3] << 24);
	return 0;
}
int pci_write_config_byte(struct pci_dev *dev, int where, u8 val)
{
	dev->lx_config[where] = val;
	return 0;
}
int pci_write_config_word(struct pci_dev *dev, int where, u16 val)
{
	dev->lx_config[where]     = (u8)(val & 0xff);
	dev->lx_config[where + 1] = (u8)(val >> 8);
	return 0;
}
int pci_write_config_dword(struct pci_dev *dev, int where, u32 val)
{
	dev->lx_config[where]     = (u8)(val & 0xff);
	dev->lx_config[where + 1] = (u8)((val >> 8) & 0xff);
	dev->lx_config[where + 2] = (u8)((val >> 16) & 0xff);
	dev->lx_config[where + 3] = (u8)((val >> 24) & 0xff);
	return 0;
}

/* Питание/пробуждение/состояние — в один-поток-мире учётные no-op. */
int  pci_save_state(struct pci_dev *dev)                          { (void)dev; return 0; }
void pci_restore_state(struct pci_dev *dev)                       { (void)dev; }
int  pci_set_power_state(struct pci_dev *dev, pci_power_t state)  { (void)dev; (void)state; return 0; }
int  pci_enable_wake(struct pci_dev *dev, pci_power_t state, bool enable)
{
	(void)dev; (void)state; (void)enable;
	return 0;
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

/* ─── доставка IRQ (Веха 72) ──────────────────────────────────────────────────
 * Один зарегистрированный обработчик на устройство. Планировщик в idle-пути спит на
 * vsys_irq_wait(cap) и по прерыванию карты зовёт handler в softirq-контексте. Регистрация живёт и
 * в сборке-«вычислялке» (Веха 68, без syscall.h) — просто никто не регистрирует IRQ (active=0),
 * и idle-путь его не трогает; сам vsys_irq_wait компилируется только под LX_HAVE_SYSCALL. */
static struct {
	int              irq;
	uintptr_t        cap;
	lx_irq_handler_t handler;
	void            *dev;
	int              active;
} lx_the_irq;

void lx_irq_register(int irq, uintptr_t cap, lx_irq_handler_t handler, void *dev)
{
	lx_the_irq.irq = irq;
	lx_the_irq.cap = cap;
	lx_the_irq.handler = handler;
	lx_the_irq.dev = dev;
	lx_the_irq.active = 1;
}

void lx_irq_unregister(int irq)
{
	(void)irq;
	lx_the_irq.active = 0;
}

void lx_sched_run(void)
{
	struct lx_task *t, *next;
	unsigned long next_exp;

	for (;;) {
		lx_jiffies_update();
		lx_timers_fire_due(); /* выстрелившие таймеры → могут сделать задачи готовыми */

		t = sched_head;
		while (t && t->state != LX_RUNNABLE && t->state != LX_INIT)
			t = t->next; /* первая готовая от головы (порядок = приоритет) */
		if (t) {
			sched_current = t;
			lx_task_run(t);
			sched_current = NULL;
			continue;
		}

		/* Готовых задач нет: если тикают таймеры — простаиваем по РЕАЛЬНОМУ времени до
		 * ближайшего срока и идём на новый круг (там он выстрелит и разблокирует задачу). */
		if (lx_timers_next(&next_exp)) {
			do
				lx_jiffies_update();
			while (time_before(jiffies, next_exp)); /* idle-ожидание тика */
			continue;
		}
#ifdef LX_HAVE_SYSCALL
		/* Таймеров нет, но зарегистрирован IRQ — уснуть на прерывании устройства (SYS_IRQ_WAIT,
		 * Веха 52): весь процесс блокируется в ядре до реального прерывания карты; по возврату
		 * зовём handler в softirq-контексте (sched_current == NULL) — он будит ждущую задачу. */
		if (lx_the_irq.active) {
			if (!vsys_irq_wait(lx_the_irq.cap)) { /* нет права/ошибка — не крутиться вхолостую */
				lx_the_irq.active = 0;
				break;
			}
			lx_the_irq.handler(lx_the_irq.irq, lx_the_irq.dev);
			continue;
		}
#endif
		/* Ни готовых задач, ни таймеров, ни IRQ — работа окончена. */
		break;
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
