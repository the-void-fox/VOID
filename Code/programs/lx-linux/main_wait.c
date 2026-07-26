/* main_wait.c — харнесс очередей ожидания и completion Lx_kit (Веха 64).
 *
 * (1) wait_event/wake_up — потребитель ждёт условие, производитель ставит и будит;
 * (2) completion — waiter ждёт wait_for_completion, worker сигналит complete;
 * (3) wait_event_timeout — ложное условие ⇒ истекает (0); разбуженное ⇒ остаток jiffies (>0).
 * Всё в одном потоке поверх планировщика (Веха 62) и таймеров (Веха 63). Обе арх.
 */
#include <stdio.h>

#include <linux/completion.h>
#include <linux/delay.h> /* msleep */
#include <linux/jiffies.h>
#include <linux/wait.h>

#include "lx_sched.h"

/* ─ Демо 1: wait_event / wake_up ─ */
static wait_queue_head_t wq;
static int data_ready;
static int consumed;

static void consumer_fn(void *arg)
{
	(void)arg;
	printf("  [1] consumer ждёт условие (data_ready)...\n");
	wait_event(wq, data_ready);
	consumed = 1;
	printf("  [1] consumer проснулся: data_ready=%d @ jiffies=%lu\n", data_ready, jiffies);
}

static void producer_fn(void *arg)
{
	(void)arg;
	msleep(20); /* дать consumer заблокироваться */
	data_ready = 1;
	printf("  [1] producer: data_ready=1, wake_up\n");
	wake_up(&wq);
}

/* ─ Демо 2: completion ─ */
static struct completion done_c;
static int completed;

static void worker_fn(void *arg)
{
	(void)arg;
	msleep(30);
	printf("  [2] worker: complete()\n");
	complete(&done_c);
}

static void waiter_fn(void *arg)
{
	(void)arg;
	printf("  [2] waiter: wait_for_completion...\n");
	wait_for_completion(&done_c);
	completed = 1;
	printf("  [2] waiter: получил completion @ jiffies=%lu\n", jiffies);
}

/* ─ Демо 3: wait_event_timeout ─ */
static wait_queue_head_t twq;
static int never;  /* остаётся 0 — условие никогда не истинно */
static int flag2;  /* станет 1 позже */
static long ret_timeout = -1, ret_signaled = -1;

static void to_waiter_fn(void *arg)
{
	(void)arg;
	ret_timeout = wait_event_timeout(twq, never, msecs_to_jiffies(30));
	printf("  [3] wait_event_timeout(ложь, 30мс) → %ld (ждём 0)\n", ret_timeout);
	ret_signaled = wait_event_timeout(twq, flag2, msecs_to_jiffies(100));
	printf("  [3] wait_event_timeout(разбужен, 100мс) → %ld (ждём >0)\n", ret_signaled);
}

static void to_setter_fn(void *arg)
{
	(void)arg;
	msleep(60); /* после истечения 1-го ожидания (~30мс), до дедлайна 2-го */
	flag2 = 1;
	printf("  [3] setter: flag2=1, wake_up\n");
	wake_up(&twq);
}

int main(void)
{
	int ok;

	printf("== Lx_kit ожидание+completion (Веха 64) ==\n");

	printf("Демо 1 — wait_event / wake_up:\n");
	init_waitqueue_head(&wq);
	lx_task_create(consumer_fn, NULL, "cons");
	lx_task_create(producer_fn, NULL, "prod");
	lx_sched_run();

	printf("Демо 2 — completion (wait_for_completion / complete):\n");
	init_completion(&done_c);
	lx_task_create(waiter_fn, NULL, "wait");
	lx_task_create(worker_fn, NULL, "work");
	lx_sched_run();

	printf("Демо 3 — wait_event_timeout (истечение и пробуждение):\n");
	init_waitqueue_head(&twq);
	lx_task_create(to_waiter_fn, NULL, "tow");
	lx_task_create(to_setter_fn, NULL, "tos");
	lx_sched_run();

	ok = consumed && completed && ret_timeout == 0 && ret_signaled > 0;
	printf("Результат: consumed=%d completed=%d timeout=%ld signaled=%ld — %s\n",
	       consumed, completed, ret_timeout, ret_signaled, ok ? "OK" : "FAIL");
	return 0;
}
