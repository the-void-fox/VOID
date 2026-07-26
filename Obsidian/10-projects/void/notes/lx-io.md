---
title: Веха 60 — linux/io.h (MMIO-аксессоры регистров)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 60 — linux/io.h: доступ к регистрам устройства (MMIO)

Второй из трио инфраструктуры (err/io/delay, Вехи 59–61). `readl`/`writel` и родня — типизированные
volatile-обращения фиксированной ширины: драйвер читает/пишет регистры устройства ИМЕННО так. Ядро
`e1000` весь построен на `er32/ew32` → `readl/writel`.

## Что наше

- **`linux/io.h`** (шим) — `readb/readw/readl/readq` + `writeb/writew/writel/writeq` (volatile,
  LE-семантика на наших LE-архах); `_relaxed`-варианты; `ioread8/16/32`/`iowrite8/16/32`;
  `phys_addr_t`/`resource_size_t`; `ioremap`/`iounmap` (в этом мире — identity).
- **`main_io.c`** (харнесс) — трактует локальный буфер как блок регистров: round-trip записи/чтения
  всех ширин (32/16/8/64) + проверка байтовой раскладки (младший байт по младшему адресу, LE) +
  `ioread/iowrite` и `readl_relaxed`.

**Про ioremap:** в «вычислительном» мире порта устройств нет, `ioremap` отдаёт адрес как есть.
НАСТОЯЩЕЕ окно регистров даёт lx_emul по MMIO-capability (`SYS_MMIO_MAP`, Вехи 51/54,
[[userspace-drivers]]/[[lx-emul-c]]) — с ним io.h сведём, когда мир порта встретится с драйверным.
Здесь важны сами аксессоры — их и проверяет харнесс.

## Сборка/запуск

`nix-build nix -A <arch>.lx_io` → мост `put … bin/<arch>/lx-io` → `run bin/lx-io`
(деривация `lx_io`: `main_io.c lx_kit.c`).

Проверено (QEMU, обе арх): `writel(0xDEADBEEF)→readl=0xDEADBEEF, LE-байт[0]=0xEF,
readq=0x0123456789ABCDEF` → «MMIO-аксессоры (readl/writel/…) верны -- io.h ядра Linux РАБОТАЮТ на
VOID», код 0. `-Wall -Wextra` чисто; ядро не менялось.

## Связано
- [[lx-err]] (Веха 59) · [[lx-delay]] (Веха 61) · [[lx-emul-c]] (ioremap по MMIO-cap — сведём) ·
  [[userspace-drivers]] · [[known-gaps]] · [[todo]]
