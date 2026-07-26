/* main_list.c — харнесс (Веха 57): наша программа VOID зовёт НЕИЗМЕНЁННЫЙ lib/list_sort.c ядра.
 *
 * list_sort.c (linux-src/, verbatim Linux 6.18.7) — устойчивая merge-сортировка двусвязного списка
 * ядра. Мы строим список нашим шимом linux/list.h (LIST_HEAD/list_add_tail/list_for_each_entry —
 * тот же API, что у ядра), зовём реальный list_sort() и проверяем: порядок верен И структура цела
 * (prev-ссылки восстановлены, список снова кольцевой). Так доказывается настоящий linux/list.h —
 * костяк ядра и любого драйвера. Следующий кирпич kit после аллокатора (Веха 56).
 */
#include <linux/list.h>      /* наш шим: struct list_head + операции */
#include <linux/list_sort.h> /* list_sort() — из настоящего ядра Linux */
#include <linux/printk.h>    /* printk() — из Lx_kit */

struct item {
	int v;
	struct list_head node;
};

/* Колбэк list_cmp_func_t: <0/0/>0. Трёхстороннее сравнение без риска переполнения. */
static int item_cmp(void *priv, const struct list_head *a, const struct list_head *b)
{
	const struct item *ia = list_entry(a, struct item, node);
	const struct item *ib = list_entry(b, struct item, node);
	(void)priv;
	return (ia->v > ib->v) - (ia->v < ib->v);
}

int main(void)
{
	static struct item items[15];
	int vals[] = { 42, 7, 99, 1, 815, 4, 7, 0, 271, 100, 3, 2, 88, 5, 13 };
	int n = (int)(sizeof(vals) / sizeof(vals[0]));
	int i, ok = 1, have_prev = 0, prev = 0, fwd = 0, bwd = 0;
	struct item *pos;
	struct list_head *lh;
	LIST_HEAD(head);

	for (i = 0; i < n; i++) {
		items[i].v = vals[i];
		list_add_tail(&items[i].node, &head); /* сохраняет порядок вставки */
	}

	list_sort(NULL, &head, item_cmp); /* ← вызов реального кода ядра Linux */

	/* Прямой обход: порядок неубывающий + счёт. */
	list_for_each_entry(pos, &head, node) {
		if (have_prev && pos->v < prev)
			ok = 0;
		prev = pos->v;
		have_prev = 1;
		fwd++;
	}
	/* Обратный обход по prev: те же элементы (проверка целостности кольца). */
	for (lh = head.prev; lh != &head; lh = lh->prev)
		bwd++;
	if (fwd != n || bwd != n) /* оба направления должны обойти ровно n узлов */
		ok = 0;

	printk("[lx-list] Linux lib/list_sort.c (6.18.7, неизменённый) на VOID: ");
	list_for_each_entry(pos, &head, node)
		printk("%d ", pos->v);
	printk("\n[lx-list] порядок+целостность %s -- список ядра Linux (list_head) РАБОТАЕТ на VOID\n",
	       ok ? "верны" : "НЕВЕРНЫ");

	return ok ? 0 : 1;
}
