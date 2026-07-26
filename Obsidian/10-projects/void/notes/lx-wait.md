---
title: Веха 64 — очереди ожидания + completion Lx_kit
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/scheduler, topic/c]
status: done
---

# Веха 64 — wait_event / wake_up / completion

Третий слой рантайма (после планировщика [[lx-sched]] и времени [[lx-timer]]): чем драйвер
ЖДЁТ асинхронного события железа — сброс, установление линка, завершение DMA/пробы. Ровно
поверх block/unblock кооперативного планировщика: ждущая задача уступает процессор, а не крутит
буси-цикл.

## Что наше

- **`linux/wait.h`** (шим) — `wait_queue_head_t`; `wait_event(wq, cond)` (блок, пока `cond`
  ложно), `wait_event_timeout(wq, cond, to)` (вернуть остаток jiffies при успехе, 0 при истечении),
  `wait_event_interruptible` (сигналов нет → тождественно, «не прерван» = 0); `wake_up`/`wake_up_all`/
  `wake_up_interruptible` (будим всегда всех). Запись ждущего `lx_wait_entry` — на СТЕКЕ задачи (как
  `wait_queue_entry` в Linux' `___wait_event`), с встроенным таймером под таймаут.
- **`linux/completion.h`** (шим) — `struct completion` (счётчик `done` + `wait_queue_head_t`);
  `init_completion`/`reinit_completion`, `complete` (+1 и разбудить), `complete_all` (`done=UINT_MAX`,
  навсегда), `wait_for_completion`/`wait_for_completion_timeout` (ждать `done>0`, затем `done--`).
  Целиком inline поверх wait_event/wake_up.
- **`lx_kit.c`** (тела): `__lx_wait(wq, e, has_deadline, deadline)` — добавить запись `e` (task=self)
  в голову списка `wq`, [взвести таймер-будильник на deadline], `lx_task_block()` (уступить),
  [снять таймер], убрать запись. `__lx_wake_up(wq)` — `lx_task_unblock` всем записям (перепроверят
  условие сами). Таймер ожидания стреляет в softirq-контексте idle-пути (Веха 63).
- **`main_wait.c`** (харнесс): (1) producer/consumer через wait_event/wake_up; (2) completion —
  waiter/worker; (3) wait_event_timeout — ложное условие истекает (0), разбуженное даёт остаток (>0).

## Как это ложится на один поток

wake_up НЕ переносит задачу «в исполнение» — только помечает готовой; планировщик подберёт её,
и она в своём `while (!cond)` перепроверит условие (истинно → выходит; ложно → снова block).
Поэтому «потерянных пробуждений» нет (условие проверяется под тем же единственным потоком до и
после block), а «ложные пробуждения» безвредны (просто ещё круг). Классический паттерн ядра,
но без единой блокировки — сериализация уже от один-поток-модели ([[lx-sched]]).

## Сборка/запуск

`nix-build nix -A <arch>.lx_wait` → мост `put … bin/<arch>/lx-wait` → `run bin/lx-wait`
(деривация `lx_wait`: `main_wait.c lx_kit.c`).

Проверено (QEMU, **обе арх**): Демо 1 — consumer просыпается @ jiffies=2 после wake_up; Демо 2 —
waiter получает completion @ jiffies≈5–6; Демо 3 — `wait_event_timeout(ложь,30мс)→0`,
`wait_event_timeout(разбужен,100мс)→7`; `Результат: consumed=1 completed=1 timeout=0 signaled=7 OK`.
`-Wall -Wextra -Wclobbered` чисто; регрессий у прочих `lx_*` нет; ядро не менялось.

## Что дальше (к e1000)

Синхронизация есть. Дальше по карте reference/dde-linux/[[30-e1000-porting-map]]:
- **workqueue + `schedule_work`/`schedule_delayed_work`** — work-задача крутит очередь работ;
  delayed — через таймер (у e1000 6.18 watchdog именно на delayed_work);
- **`module_init`-редирект + генератор заглушек** ([[20-linker-driven-workflow]]);
- **driver-model/PCI** → **netdev-подмножество** → `e1000_hw.c` → `e1000_main.c`.

## Связано
- [[lx-sched]] (Веха 62, планировщик) · [[lx-timer]] (Веха 63, таймеры — под таймаут-ожидания) ·
  [[lx-kit]] (аллокатор) · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[10-lx-kit-runtime]] · [[30-e1000-porting-map]]
