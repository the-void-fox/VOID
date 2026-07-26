/* main_work.c — харнесс рабочих очередей Lx_kit (Веха 65).
 *
 * (1) schedule_work — две работы в системную очередь, FIFO-порядок, flush_work ждёт исполнения;
 * (2) schedule_delayed_work — работа через задержку (~30 мс), flush_delayed_work;
 * (3) cancel_delayed_work_sync — снять до срабатывания: работа НЕ должна выполниться.
 * Всё в одном потоке поверх планировщика (62), таймеров (63) и ожидания (64). Обе арх.
 */
#include <stdio.h>

#include <linux/delay.h> /* msleep */
#include <linux/jiffies.h>
#include <linux/workqueue.h>

#include "lx_sched.h"

static int order_log[8];
static int order_n;

/* ─ Демо 1: schedule_work ─ */
static struct work_struct w_a, w_b;
static void wa_fn(struct work_struct *w) { (void)w; printf("  [1] work A\n"); order_log[order_n++] = 1; }
static void wb_fn(struct work_struct *w) { (void)w; printf("  [1] work B\n"); order_log[order_n++] = 2; }

/* ─ Демо 2: schedule_delayed_work ─ */
static struct delayed_work dw;
static unsigned long dw_at;
static void dw_fn(struct work_struct *w)
{
	(void)w;
	dw_at = jiffies;
	printf("  [2] delayed work @ jiffies=%lu\n", jiffies);
	order_log[order_n++] = 3;
}

/* ─ Демо 3: cancel до срабатывания ─ */
static struct delayed_work dwc;
static int cancel_ran;
static void wc_fn(struct work_struct *w)
{
	(void)w;
	cancel_ran = 1;
	printf("  [3] ОШИБКА: снятая work выполнилась!\n");
}

static void driver_fn(void *arg)
{
	unsigned long j0;
	bool was;

	(void)arg;

	printf("Демо 1 — schedule_work (FIFO A,B; flush_work ждёт):\n");
	INIT_WORK(&w_a, wa_fn);
	INIT_WORK(&w_b, wb_fn);
	schedule_work(&w_a);
	schedule_work(&w_b);
	flush_work(&w_a);
	flush_work(&w_b);

	printf("Демо 2 — schedule_delayed_work (30 мс):\n");
	j0 = jiffies;
	INIT_DELAYED_WORK(&dw, dw_fn);
	schedule_delayed_work(&dw, msecs_to_jiffies(30));
	flush_delayed_work(&dw);
	printf("  [2] задержка ~%lu jiffies (ждём ~3)\n", dw_at - j0);

	printf("Демо 3 — cancel_delayed_work_sync до срабатывания (50 мс):\n");
	INIT_DELAYED_WORK(&dwc, wc_fn);
	schedule_delayed_work(&dwc, msecs_to_jiffies(50));
	was = cancel_delayed_work_sync(&dwc);
	printf("  [3] cancel вернул %d (ждём 1 = было заведено)\n", was);
	msleep(80); /* окно срабатывания прошло — снятая работа НЕ должна выполниться */
	printf("  [3] после 80 мс: снятая work выполнилась=%d (ждём 0)\n", cancel_ran);
}

int main(void)
{
	int ok;

	printf("== Lx_kit рабочие очереди (Веха 65) ==\n");
	lx_task_create(driver_fn, NULL, "drv");
	lx_sched_run();

	ok = order_n == 3 && order_log[0] == 1 && order_log[1] == 2 && order_log[2] == 3 && !cancel_ran;
	printf("Результат: порядок=%d%d%d cancel_ran=%d — %s\n",
	       order_log[0], order_log[1], order_log[2], cancel_ran, ok ? "OK" : "FAIL");
	return 0;
}
