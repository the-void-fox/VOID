/* main_sched.c — харнесс кооперативного планировщика Lx_kit (Веха 62).
 *
 * Доказывает костяк рантайма dde_linux: несколько задач в ОДНОМ потоке, каждая на своём
 * стеке, переключаются кооперативно. Демо A — round-robin по yield; Демо B — ping/pong по
 * block/unblock (модель wait_event/wake_up). Печать адреса локали доказывает, что стеки
 * разные. Чистая вычислялка, обе арх (riscv64 + x86_64). См. reference/dde-linux/10-lx-kit-runtime.md.
 */
#include <stdio.h>

#include "lx_sched.h"

/* Демо A: задача-счётчик печатает шаг и уступает — round-robin поверх отдельных стеков. */
static void counter_fn(void *arg)
{
	const char *name = (const char *)arg;
	char marker = 0; /* адрес локали ≈ «стек этой задачи»; у каждой задачи он свой */
	int i;

	for (i = 0; i < 3; i++) {
		printf("  [A] %s шаг %d (стек≈%p)\n", name, i, (void *)&marker);
		lx_sched_yield();
	}
}

/* Демо B: ping/pong через block/unblock — точная модель wait_event/wake_up драйвера. */
static struct lx_task *t_ping, *t_pong;
static const int pp_rounds = 3;
static int pp_seq; /* контроль строгого чередования: ждём 121212 */

static void ping_fn(void *arg)
{
	int i;

	(void)arg;
	lx_sched_yield(); /* дать pong стартовать и уйти в block до первого ping */
	for (i = 0; i < pp_rounds; i++) {
		printf("  [B] ping %d\n", i);
		pp_seq = pp_seq * 10 + 1;
		lx_task_unblock(t_pong); /* pong ждёт в block → делаем готовым */
		lx_task_block();         /* сами ждём ответа pong */
	}
}

static void pong_fn(void *arg)
{
	int i;

	(void)arg;
	for (i = 0; i < pp_rounds; i++) {
		lx_task_block(); /* ждём ping */
		printf("  [B] pong %d\n", i);
		pp_seq = pp_seq * 10 + 2;
		lx_task_unblock(t_ping);
	}
}

int main(void)
{
	printf("== Lx_kit планировщик (Веха 62): кооперативные задачи, один поток ==\n");

	printf("Демо A — round-robin по yield (3 задачи x 3 шага, ждём A0 B0 C0 A1 …):\n");
	lx_task_create(counter_fn, "A", "cntA");
	lx_task_create(counter_fn, "B", "cntB");
	lx_task_create(counter_fn, "C", "cntC");
	lx_sched_run();

	printf("Демо B — ping/pong по block/unblock (%d раундов, модель wait_event/wake_up):\n",
	       pp_rounds);
	t_ping = lx_task_create(ping_fn, NULL, "ping");
	t_pong = lx_task_create(pong_fn, NULL, "pong");
	lx_sched_run();

	printf("Результат: чередование pp_seq=%d (ждём 121212) — %s\n",
	       pp_seq, pp_seq == 121212 ? "OK" : "FAIL");
	return 0;
}
