---
title: Веха 63 — jiffies + таймеры Lx_kit (уступающий msleep)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/scheduler, topic/c]
status: done
---

# Веха 63 — jiffies + таймеры: время поверх планировщика

Второй слой рантайма (после планировщика [[lx-sched]]): счётчик тиков `jiffies` и одноразовые
таймеры `timer_list`. Substrate под watchdog'и драйверов, `delayed_work`/workqueue (следующая
веха) и таймауты опроса. Заодно **`msleep` стал уступающим** — первый видимый плод связки
время+планировщик.

## Что наше

- **`linux/jiffies.h`** (шим) — `HZ=100` (1 jiffy = 10 мс), глобальный `jiffies` (volatile),
  `time_after`/`time_before`/`…_eq` (знаковая разность — корректны при переполнении),
  `msecs_to_jiffies`/`jiffies_to_msecs`/`usecs_to_jiffies` (округление ВВЕРХ — сон не короче).
- **`linux/timer.h`** (шим) — `struct timer_list` (шимовая: `expires`/`function` + приватная
  линковка `lx_*`), `timer_setup`/`mod_timer`/`add_timer`/`timer_delete`/`timer_delete_sync`/
  `timer_pending`, `from_timer`/`timer_container_of`. Легаси-имена `del_timer[_sync]` — алиасы.
- **`lx_kit.c`** (тела):
  - `jiffies` двигается по **монотонному времени VOID** (`now_us`, точка отсчёта — ленивая):
    `jiffies = (now_us - boot_us) / (1e6/HZ)`; обновляется в точках планирования/задержки.
  - Очередь таймеров (односвязная), `lx_timers_fire_due()` — стреляет всеми, чей срок наступил
    (снимает ДО коллбэка — тот вправе перевзвести); `lx_timers_next()` — ближайший срок.
  - **Цикл планировщика** (`lx_sched_run`) в **idle-пути**: если готовых задач нет, но тикают
    таймеры — простаивает по реальному времени до ближайшего срока и идёт на круг стрелять
    (коллбэк разблокирует задачу). Таймеры бьют в **softirq-контексте**: `lx_task_self()` там
    `NULL` (как в Linux — таймеры не в контексте процесса).
  - **`msleep`** переписан: в контексте задачи ставит таймер, который её разбудит, и
    `lx_task_block()` (уступает); вне задачи — прежний буси-`mdelay`.
- **`main_timer.c`** (харнесс): Демо A — две сони по `msleep(20)` ЧЕРЕДУЮТСЯ (msleep уступает),
  jiffies растёт; Демо B — три таймера заведены вразброс (30/10/20 мс), стреляют по ВОЗРАСТАНИЮ
  expires (`2 3 1`).

## Сборка/запуск

`nix-build nix -A <arch>.lx_timer` → мост `put … bin/<arch>/lx-timer` → `run bin/lx-timer`
(деривация `lx_timer`: `main_timer.c lx_kit.c`).

Проверено (QEMU, **обе арх идентично**): `HZ=100, jiffy=10 мс`;
- Демо A: `S1 тик0@0, S2 тик0@0, S1 тик1@2, S2 тик1@2, S1 тик2@4, S2 тик2@4` — msleep уступает,
  сони чередуются, jiffies растёт синхронно.
- Демо B: `порядок: 2 3 1 @ jiffies 7 8 9`, `Результат: порядок=231 OK`.
- `-Wall -Wextra -Wclobbered` чисто; регрессий у прочих `lx_*` нет; ядро не менялось.

## Оговорки (в [[known-gaps]])

- jiffies двигается в **точках планирования/задержки**, не непрерывно: буси-цикл, читающий
  `jiffies` без `msleep`/`udelay`/`cpu_relax`, время не увидит (реальные таймаут-циклы драйверов
  зовут задержку внутри — ок).
- idle-ожидание таймера — **буси** по реальному времени (жжёт хост-CPU, пока все задачи спят);
  настоящий сон ядра VOID подключим позже.

## Что дальше (к e1000)

Время есть. Дальше по карте reference/dde-linux/[[30-e1000-porting-map]]:
- **`wait_event`/`wake_up`/completion** поверх block/unblock (условие + пробуждение);
- **workqueue + `schedule_delayed_work`** (у e1000 6.18 watchdog именно на delayed_work) —
  work-задача + таймер;
- **module_init-редирект + генератор заглушек** ([[20-linker-driven-workflow]]) → driver-model/PCI
  → netdev-подмножество → `e1000_hw.c` → `e1000_main.c`.

## Связано
- [[lx-sched]] (Веха 62, планировщик — основа) · [[lx-delay]] (Веха 61, буси-паузы; `msleep` теперь
  уступает) · [[lx-kit]] (аллокатор) · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[10-lx-kit-runtime]] · [[30-e1000-porting-map]]
