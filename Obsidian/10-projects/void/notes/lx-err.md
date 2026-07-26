---
title: Веха 59 — linux/err.h (идиома «ошибка в указателе»)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 59 — linux/err.h: ошибка, закодированная в указателе

Инфраструктурный кирпич kit (после трёх столпов: аллокатор [[lx-kit]], список [[lx-list]], биты
[[lx-bits]]). Идиома ядра: функции возвращают ЛИБО настоящий указатель, ЛИБО `ERR_PTR(-Exxx)` —
маленькое отрицательное значение (код `errno`) как невалидный указатель верхней страницы адресов,
без отдельного out-параметра. Драйверы (и `e1000`) так возвращают ресурсы повсюду.

## Что наше

- **`linux/err.h`** (шим) — ровно ядровый набор: `ERR_PTR`/`PTR_ERR`/`IS_ERR`/`IS_ERR_OR_NULL`/
  `PTR_ERR_OR_ZERO`/`ERR_CAST` + `IS_ERR_VALUE`/`MAX_ERRNO` (4095). Тем же приёмом, что ядро.
- **`linux/errno.h`** (шим) — тонкий проход к newlib `<errno.h>` (EINVAL/ENOMEM/… — те же имена/значения).
- **`main_err.c`** (харнесс) — прямая проверка семантики: `ERR_PTR(-ENOMEM)` распознаётся `IS_ERR`
  и извлекается `PTR_ERR`; настоящий указатель — не ошибка; `NULL` ловится `IS_ERR_OR_NULL`.

`err.h` не привязан к отдельному `.c` ядра — это инфраструктура; проверяется харнессом, а в бою
раскроется, когда портируемый драйвер станет возвращать `ERR_PTR`. Часть трио err/io/delay (Вехи
59–61), готовящего kit к реальному драйверу.

## Сборка/запуск

`nix-build nix -A <arch>.lx_err` → мост `put … bin/<arch>/lx-err` → `run bin/lx-err`
(деривация `lx_err`: `main_err.c lx_kit.c` — `lx_kit.c` ради `printk`).

Проверено (QEMU, обе арх): `ERR_PTR(-ENOMEM) → IS_ERR=1 PTR_ERR=-12; valid → IS_ERR=0; NULL →
IS_ERR_OR_NULL=1` → «идиома «ошибка в указателе» верна -- err.h ядра Linux РАБОТАЕТ на VOID», код 0.
`-Wall -Wextra` чисто; ядро не менялось.

## Связано
- [[lx-io]] (Веха 60, MMIO) · [[lx-delay]] (Веха 61, паузы) · [[lx-bits]] (Веха 58) · [[known-gaps]] · [[todo]]
