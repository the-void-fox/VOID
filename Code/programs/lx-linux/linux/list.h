/* linux/list.h — ШИМ lx_emul (Веха 57), НЕ исходник Linux.
 *
 * Двусвязный кольцевой список — костяк ядра и любого драйвера (очереди netdev, списки буферов,
 * цепочки устройств…). Даём тот же тип struct list_head и привычный набор операций
 * (INIT_LIST_HEAD/list_add/list_add_tail/list_del/list_empty + обходы list_for_each_entry),
 * реализованные ровно как в ядре. Здесь — распространённое подмножество под портируемый код; растёт.
 */
#ifndef _LINUX_LIST_H_SHIM
#define _LINUX_LIST_H_SHIM

#include <linux/container_of.h>
#include <linux/types.h>

struct list_head {
	struct list_head *next, *prev;
};

#define LIST_HEAD_INIT(name) { &(name), &(name) }
#define LIST_HEAD(name) struct list_head name = LIST_HEAD_INIT(name)

static inline void INIT_LIST_HEAD(struct list_head *list)
{
	list->next = list;
	list->prev = list;
}

static inline void __list_add(struct list_head *new_,
			      struct list_head *prev, struct list_head *next)
{
	next->prev = new_;
	new_->next = next;
	new_->prev = prev;
	prev->next = new_;
}

/* Вставка сразу после head (стек/начало). */
static inline void list_add(struct list_head *new_, struct list_head *head)
{
	__list_add(new_, head, head->next);
}

/* Вставка перед head (очередь/конец) — сохраняет порядок вставки при обходе. */
static inline void list_add_tail(struct list_head *new_, struct list_head *head)
{
	__list_add(new_, head->prev, head);
}

static inline void __list_del(struct list_head *prev, struct list_head *next)
{
	next->prev = prev;
	prev->next = next;
}

static inline void list_del(struct list_head *entry)
{
	__list_del(entry->prev, entry->next);
	entry->next = NULL;
	entry->prev = NULL;
}

static inline int list_empty(const struct list_head *head)
{
	return head->next == head;
}

#define list_entry(ptr, type, member)       container_of(ptr, type, member)
#define list_first_entry(ptr, type, member) list_entry((ptr)->next, type, member)
#define list_next_entry(pos, member) \
	list_entry((pos)->member.next, typeof(*(pos)), member)

#define list_for_each(pos, head) \
	for ((pos) = (head)->next; (pos) != (head); (pos) = (pos)->next)

#define list_for_each_entry(pos, head, member)                          \
	for ((pos) = list_first_entry(head, typeof(*(pos)), member);    \
	     &(pos)->member != (head);                                  \
	     (pos) = list_next_entry(pos, member))

#endif /* _LINUX_LIST_H_SHIM */
