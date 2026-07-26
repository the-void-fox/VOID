---
title: Веха 57 — linux/list.h (двусвязный список ядра), реальный list_sort.c работает
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 57 — костяк ядра: двусвязный список (linux/list.h)

Следующий кирпич kit после аллокатора ([[lx-kit]], Веха 56): **двусвязный кольцевой список** —
структура, на которой стоит всё ядро и любой драйвер (очереди netdev, списки буферов, цепочки
устройств). Доказываем настоящий `linux/list.h` неизменённым `lib/list_sort.c` из Linux 6.18.7 —
устойчивой merge-сортировкой списка.

## Что портировано (verbatim) и что наше

- **`linux-src/list_sort.c`** — **НЕИЗМЕНЁННЫЙ** `lib/list_sort.c` из Linux **6.18.7** (GPL-2.0,
  SPDX на месте, 257 строк): устойчивая восходящая merge-сортировка на `struct list_head`
  (2:1-сбалансированные слияния, обход `->next/->prev` напрямую). Ни строчки не тронуто.
- **`linux/list_sort.h`** — **НЕИЗМЕНЁННЫЙ** `include/linux/list_sort.h` (GPL-2.0): тип колбэка
  `list_cmp_func_t` + прототип `list_sort()`.
- **`linux/list.h`** (наш шим) — тип `struct list_head` + распространённое подмножество операций
  ровно как в ядре: `LIST_HEAD`/`INIT_LIST_HEAD`, `list_add`/`list_add_tail`, `list_del`,
  `list_empty`, `list_entry`/`list_first_entry`/`list_next_entry`, обходы `list_for_each`/
  `list_for_each_entry`. Растёт по мере надобности.
- **`linux/compiler.h`** (наш шим) — обёртки компилятора: `likely`/`unlikely` (через
  `__builtin_expect`), `barrier()`, `READ_ONCE`/`WRITE_ONCE`, пустые sparse-аннотации
  `__user`/`__iomem`/… — база почти под весь код ядра.
- **`linux/container_of.h`** (наш шим) — `container_of()` вынесен из `kernel.h` в свой заголовок
  (как в ядре 6.x), чтобы `list.h` опирался на него, не таща весь «зонтик» `kernel.h`.
- **`main_list.c`** (наш харнесс) — строит список нашим `list.h` (`list_add_tail`), зовёт реальный
  `list_sort()`, проверяет **порядок И целостность**: прямой обход неубывающий + обратный по
  `prev` обходит ровно `n` узлов (кольцо восстановлено). Печать — ядровым `printk`.

## Тонкость

`list_sort.c` сам НЕ использует макросы `list.h` — он крутит `->next/->prev` напрямую (для скорости
временно рвёт `prev`-ссылки, восстанавливая их в финальном `merge_final`). Поэтому полноту `list.h`
(LIST_HEAD/list_add_tail/list_for_each_entry) прогоняет именно **харнесс** — вместе они доказывают
настоящий `list.h`. `container_of` переехал в свой заголовок — `kernel.h` теперь его включает
(регрессии `lx_sort`/`lx_argv` нет).

## Сборка/доставка/запуск (C-мир, как [[lx-linux]])

```
nix-build nix -A <arch>.lx_list     # list_sort.c + list.h/compiler.h/container_of.h + harness → ELF
void-store-import void-disk.img put result/bin/lx-list bin/<arch>/lx-list
run bin/lx-list                     # в vsh; чистая вычислялка (список — без аллокатора), обе арх
```
`nix/default.nix` — деривация `lx_list` (`$CC … -I. -DCONFIG_64BIT -static main_list.c
linux-src/list_sort.c`; `lx_kit.c` не нужен — список без аллокатора). ELF на диске.

Проверено (QEMU, обе арх): `run bin/lx-list` →
`[lx-list] Linux lib/list_sort.c (6.18.7, неизменённый) на VOID: 0 1 2 3 4 5 7 7 13 42 88 99 100 271 815`
→ `[lx-list] порядок+целостность верны -- список ядра Linux (list_head) РАБОТАЕТ на VOID`, код 0.
Предупреждений нет (`-Wall -Wextra`, обе арх). Ядро не менялось.

## Что дальше (к драйверу e1000)

Есть аллокатор (Веха 56) и список (Веха 57) — два столпа. Дальше по арке:
- ещё утиль/sync ядра: `err.h` (ERR_PTR/IS_ERR), `bitops.h`, `spinlock`/`mutex`, `jiffies`/`delay`,
  `io.h`, `atomic` — по мере файлов;
- **Lx_kit-рантайм**: timer/workqueue/completion/wait поверх нитей+futex (основа в [[lx-emul-c]]),
  свести с ioremap/DMA/`request_irq`;
- **driver-model + PCI** → **netdev-подмножество** (alloc_etherdev/netif/sk_buff/NAPI), чтобы
  скомпилировался `e1000.h` (тянет весь сетевой стек ядра);
- **сам драйвер** `drivers/net/ethernet/intel/e1000/*.c` (e1000_hw.c → e1000_main.c). Закроет
  «длинный хвост» железа X54C (Atheros/EHCI/wifi).

Лицензии: vendored-файлы Linux — под GPL-2.0 (свои SPDX); шимы/харнесс — код проекта.

## Файлы

- `programs/lx-linux/linux-src/list_sort.c`, `linux/list_sort.h` — verbatim Linux 6.18.7 (GPL-2.0).
- `programs/lx-linux/linux/{list,compiler,container_of}.h` — новые/вынесенные шим-заголовки.
- `programs/lx-linux/main_list.c` — харнесс; `nix/default.nix` — деривация `lx_list`.

## Связано
- [[lx-kit]] (Веха 56, аллокатор) · [[lx-linux]] (Веха 55, sort.c — старт конвейера) ·
  [[lx-emul-c]] (C-каркас: ioremap/DMA/IRQ — свести с Lx_kit) · [[store-bridge]] (доставка) ·
  [[known-gaps]] · [[todo]]
