---
title: Веха 65 — рабочие очереди Lx_kit (workqueue, delayed_work)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/scheduler, topic/c]
status: done
---

# Веха 65 — workqueue: отложенная работа

Четвёртый слой рантайма (планировщик [[lx-sched]] → время [[lx-timer]] → ожидание [[lx-wait]] →
**workqueue**). Драйвер выносит «тяжёлое» из IRQ/таймера в контекст, где МОЖНО спать. У **e1000
6.18** watchdog реализован именно на `delayed_work` (не на raw `timer_list`) — этот слой закрывает
последний рантайм-примитив перед driver-model/PCI.

## Что наше

- **`linux/workqueue.h`** (шим) — `struct work_struct`/`struct delayed_work` (шимовые: `func` +
  приватная линковка `lx_*`); `INIT_WORK`/`INIT_DELAYED_WORK`, `to_delayed_work`, `work_pending`;
  `schedule_work`/`schedule_delayed_work`/`queue_work`/`queue_delayed_work`/`mod_delayed_work`;
  `flush_work`/`flush_delayed_work`/`flush_workqueue`; `cancel_work_sync`/`cancel_delayed_work[_sync]`;
  `alloc_workqueue`/`create_singlethread_workqueue`/`destroy_workqueue`. Системная очередь
  `system_wq` — ленивый геттер (драйвер лишь ЧИТАЕТ символ → `#define system_wq lx_get_system_wq()`).
- **`lx_kit.c`** (тела): каждую очередь обслуживает **задача-воркер** (`lx_worker_fn`): крутит
  очередь работ (`func(work)` — в контексте задачи, МОЖНО спать), пусто → `lx_task_block()`.
  `queue_work` кладёт работу в хвост и `lx_task_unblock(worker)`. `delayed_work` — через таймер
  (Веха 63): по срабатыванию коллбэк `queue_work`. `flush_*` — `wait_event` на flush-очереди
  воркера (он будит её после каждой работы). `cancel_*` — снять из очереди/разоружить таймер;
  «sync» тривиален (один поток: воркер не может исполнять работу, пока идёт заказчик).
- **`main_work.c`** (харнесс): (1) две работы FIFO через `schedule_work`, `flush_work` ждёт; (2)
  `schedule_delayed_work(30 мс)` + `flush_delayed_work`; (3) `cancel_delayed_work_sync` до
  срабатывания — работа НЕ выполняется.

## Сборка/запуск

`nix-build nix -A <arch>.lx_work` → мост `put … bin/<arch>/lx-work` → `run bin/lx-work`
(деривация `lx_work`: `main_work.c lx_kit.c`).

Проверено (QEMU, **обе арх**): Демо 1 — `work A`, `work B` (FIFO); Демо 2 — `delayed work @
jiffies≈3–5` (задержка ~3 jiffies); Демо 3 — `cancel вернул 1`, снятая работа `выполнилась=0`;
`Результат: порядок=123 cancel_ran=0 OK`. `-Wall -Wextra -Wclobbered` чисто; регрессий у прочих
`lx_*` нет; ядро не менялось.

## Оговорка (в [[known-gaps]])

Задача-воркер — демон (бесконечный цикл), при остановке планировщика остаётся заблокированной;
её стек не реапится (для процесса-харнесса неважно; в реальном драйвере воркер живёт всегда).
`destroy_workqueue` пока лишь flush'ит (воркера не гасит).

## Что дальше (к e1000)

Рантайм-примитивы Lx_kit готовы (память/список/биты/err/io/delay + планировщик/таймеры/
ожидание/workqueue). Дальше — переход к САМОМУ драйверу по карте reference/dde-linux/[[30-e1000-porting-map]]:
- **`module_init`-редирект + генератор заглушек** ([[20-linker-driven-workflow]]) — компилировать
  реальный `.c`, гасить неопределённые символы;
- **driver-model/PCI** (`linux/device.h`, `linux/pci.h`) поверх нашего PCI-обхода + MMIO/DMA-cap;
- **netdev-подмножество** (`netdevice.h`/`skbuff.h`/`etherdevice.h`/`dma-mapping.h`) → `e1000_hw.c`
  → `e1000_main.c`.

## Связано
- [[lx-sched]] (Веха 62) · [[lx-timer]] (Веха 63, таймеры — под delayed_work) · [[lx-wait]] (Веха 64,
  wait_event — под flush) · [[lx-kit]] (аллокатор) · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[10-lx-kit-runtime]] · [[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
