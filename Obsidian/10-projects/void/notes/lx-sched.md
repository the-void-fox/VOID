---
title: Веха 62 — кооперативный планировщик Lx_kit (setjmp/longjmp, один поток)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/scheduler, topic/c]
status: done
---

# Веха 62 — костяк рантайма Lx_kit: кооперативный планировщик

Первый кусок **рантайма** (а не очередной шим-заголовок): то, поверх чего дальше лягут
jiffies/таймеры, `wait_event`/`wake_up`, workqueue, `request_irq`, kthread. Модель — как у
Genode dde_linux (`lx_kit/{task,scheduler}.cc`): весь портируемый код ядра Linux исполняется
в **одном потоке**, задачи переключаются только в явных точках. Обоснование выбора (вместо
первоначального «threads + futex») — в reference/dde-linux/[[10-lx-kit-runtime]].

## Почему один поток — это выигрыш

- Внутри портированного Linux-кода **нет вытеснения** → нет гонок; `spinlock`/`mutex`/атомики
  почти no-op (снимает оговорку Вехи 58 про неатомарный `set_bit` для драйверного контекста).
- newlib `_impure_ptr` (stdio) один — конкурентного доступа нет, всё сериализовано.
- Детерминированная отладка: порядок исполнения воспроизводим.
- Настоящий атомарный флаг нужен ТОЛЬКО на стыке реального IRQ (наш `SYS_IRQ_WAIT`, [[irq]]).

## Что наше

- **`lx_sched.h`** — API рантайма: `struct lx_task` (два `jmp_buf`: `env` — точка возобновления
  задачи, `saved_env` — возврат в планировщик), `lx_task_create[_typed]`, `lx_sched_run`,
  `lx_sched_yield`, `lx_task_block`/`lx_task_unblock`, `lx_sched_wake_type`, `lx_task_self`.
- **`lx_kit.c`** (тела):
  - **`arch_execute(sp, fn)`** — короткая арх-вставка: сменить SP на стек задачи и вызвать `fn`
    (riscv64: `mv sp,x; jalr`; x86_64: `movq …,%rsp; call *`). Назад не возвращается — задача
    уходит через `longjmp`. Единственная арх-специфика вехи.
  - Задача = отдельный стек (32 КиБ `malloc`, вершина 16-выровнена) + `setjmp/longjmp`.
    Первый запуск: `arch_execute` → трамплин `func(arg)` на своём стеке → по возврату `LX_DEAD`
    → `longjmp(saved_env)`. Возобновление: `longjmp(env)`. Уступка/блок: `setjmp(env)` +
    `longjmp(saved_env)`.
  - Планировщик `lx_sched_run`: односвязная очередь, «первая готовая от головы» (порядок =
    приоритет); `yield` перекладывает задачу в хвост (round-robin среди равных); реап `LX_DEAD`
    — после остановки цикла (чтобы указатели на задачи не висли, пока другая может к ним обратиться).
- **`main_sched.c`** (харнесс): Демо A — 3 задачи-счётчика round-robin по `yield` (печать адреса
  локали доказывает РАЗНЫЕ стеки); Демо B — ping/pong по `block`/`unblock` (модель `wait_event`/
  `wake_up`), контроль строгого чередования `pp_seq == 121212`.

## Сборка/запуск

`nix-build nix -A <arch>.lx_sched` → мост `put … bin/<arch>/lx-sched` → `run bin/lx-sched`
(деривация `lx_sched`: `main_sched.c lx_kit.c`).

Проверено (QEMU, **обе арх**):
- Демо A: `A0 B0 C0 A1 B1 C1 A2 B2 C2` — честный round-robin; три СТАБИЛЬНЫХ различных адреса
  стека (x86 ≈ 0x…849f/…056f/…863f, ~28 КиБ врозь) → отдельные стеки работают.
- Демо B: `ping0 pong0 … pong2`, `pp_seq=121212 OK` — кооперативное переключение через block/unblock.
- `-Wall -Wextra -Wclobbered` чисто (риск setjmp снят); регрессий у прочих `lx_*` нет; ядро не менялось.

## Что дальше (к e1000)

Костяк есть. Дальше растим `lx_kit.c` по карте reference/dde-linux/[[30-e1000-porting-map]]:
- **jiffies + очередь таймеров** (`timer_list`/`mod_timer`) на `LX_TASK_TIME`-задаче;
- **`wait_event`/`wake_up`/`schedule_work`** поверх block/unblock; `msleep` → уступающий сон
  (сейчас буси, Веха 61);
- **`module_init`-редирект + генератор заглушек** (линкер-ориентированно, [[20-linker-driven-workflow]]);
- **PCI/driver-model** → **netdev-подмножество + skb + dma-mapping** → `e1000_hw.c` → `e1000_main.c`.

## Связано
- [[lx-kit]] (Веха 56, аллокатор) · [[lx-delay]] (Веха 61, буси-паузы) · [[irq]] (SYS_IRQ_WAIT —
  стык реального прерывания) · [[threads]] (нити/futex VOID — вне Lx_kit) · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[00-overview]] · [[10-lx-kit-runtime]] · [[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
