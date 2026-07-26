/* main_timer.c — харнесс jiffies + таймеров Lx_kit (Веха 63).
 *
 * Проверяет: (A) `msleep` теперь УСТУПАЕТ — две задачи-сони чередуются, jiffies растёт;
 * (B) очередь таймеров — три таймера стреляют по ВОЗРАСТАНИЮ expires (не в порядке завода).
 * Всё в одном потоке поверх кооперативного планировщика (Веха 62). Обе арх (riscv64 + x86_64).
 * См. reference/dde-linux/10-lx-kit-runtime.md.
 */
#include <stdio.h>

#include <linux/delay.h> /* msleep */
#include <linux/jiffies.h>
#include <linux/timer.h>

#include "lx_sched.h"

/* ─ Демо A: msleep уступает — сони чередуются ─ */
static void sleeper_fn(void *arg)
{
	const char *name = (const char *)arg;
	int i;

	for (i = 0; i < 3; i++) {
		printf("  [A] %s тик %d @ jiffies=%lu\n", name, i, jiffies);
		msleep(20); /* уступает планировщику: пока спим, бежит другая соня */
	}
}

/* ─ Демо B: таймеры стреляют по возрастанию expires ─ */
static int fire_order[4];
static unsigned long fire_at[4];
static int fired_n;

struct demo_timer {
	struct timer_list t;
	int id;
};
static struct demo_timer T1, T2, T3;

static void demo_cb(struct timer_list *tl)
{
	struct demo_timer *d = from_timer(d, tl, t);

	fire_order[fired_n] = d->id;
	fire_at[fired_n] = jiffies;
	fired_n++;
}

static void arm_fn(void *arg)
{
	unsigned long j0 = jiffies;

	(void)arg;
	/* заводим ВРАЗБРОС: T1 позже всех, T2 раньше всех, T3 посередине */
	timer_setup(&T1.t, demo_cb, 0); T1.id = 1; mod_timer(&T1.t, j0 + msecs_to_jiffies(30));
	timer_setup(&T2.t, demo_cb, 0); T2.id = 2; mod_timer(&T2.t, j0 + msecs_to_jiffies(10));
	timer_setup(&T3.t, demo_cb, 0); T3.id = 3; mod_timer(&T3.t, j0 + msecs_to_jiffies(20));

	while (fired_n < 3)
		msleep(5); /* уступаем, пока все три не выстрелят */

	printf("  [B] порядок: %d %d %d @ jiffies %lu %lu %lu (ждём 2 3 1)\n",
	       fire_order[0], fire_order[1], fire_order[2], fire_at[0], fire_at[1], fire_at[2]);
}

int main(void)
{
	printf("== Lx_kit таймеры+jiffies (Веха 63): HZ=%d, jiffy=%u мс ==\n", HZ, jiffies_to_msecs(1));

	printf("Демо A — msleep УСТУПАЕТ (2 сони x 3 тика, чередуются, jiffies растёт):\n");
	lx_task_create(sleeper_fn, "S1", "s1");
	lx_task_create(sleeper_fn, "S2", "s2");
	lx_sched_run();

	printf("Демо B — очередь таймеров (стреляют по возрастанию expires, не завода):\n");
	lx_task_create(arm_fn, NULL, "arm");
	lx_sched_run();

	printf("Результат: порядок=%d%d%d (ждём 231) — %s\n",
	       fire_order[0], fire_order[1], fire_order[2],
	       (fire_order[0] == 2 && fire_order[1] == 3 && fire_order[2] == 1) ? "OK" : "FAIL");
	return 0;
}
