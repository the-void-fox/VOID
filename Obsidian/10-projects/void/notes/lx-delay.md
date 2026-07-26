---
title: Веха 61 — linux/delay.h (задержки тайминга железа)
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 61 — linux/delay.h: паузы для тайминга железа

Третий из трио инфраструктуры (err/io/delay, Вехи 59–61). `udelay`/`mdelay`/`ndelay` — БУСИ-паузы
(в ядре зовутся и в атомарном контексте, спать нельзя): драйвер выдерживает их между записью
управляющего регистра и опросом статуса (сброс/линк/EEPROM у e1000 — сплошь такие паузы).

## Что наше

- **`linux/delay.h`** (шим) — объявления `ndelay`/`udelay`/`mdelay`/`msleep`.
- **`lx_kit.c`** (Lx_kit-рантайм) — тела: БУСИ-ожидание по **монотонному времени VOID**
  (`gettimeofday` → `vsys_ticks`, разрешение 1–100 нс/тик): крутимся, пока не прошло запрошенное.
  `mdelay` = цикл `udelay(1000)`; `ndelay` округляет вверх до мкс; `msleep` пока тоже буси
  (уступающий сон — с планировщиком Lx_kit позже).
- **`main_delay.c`** (харнесс) — замеряет `gettimeofday` до/после реальной паузы и проверяет, что
  прошло НЕ МЕНЬШЕ запрошенного (верхнюю границу не проверяем — QEMU/TCG может тормозить).

## Сборка/запуск

`nix-build nix -A <arch>.lx_delay` → мост `put … bin/<arch>/lx-delay` → `run bin/lx-delay`
(деривация `lx_delay`: `main_delay.c lx_kit.c` — тела задержек + printk из Lx_kit).

Проверено (QEMU, обе арх): `mdelay(50 мс)` занял ~50131/50402 мкс, `udelay(3000 мкс)` — ~3012/3037
мкс → «задержки (буси-ожидание по монотонному времени) верны -- delay.h ядра Linux РАБОТАЕТ на
VOID», код 0. Паузы РЕАЛЬНО состоялись (замер по монотонному источнику). `-Wall -Wextra` чисто;
ядро не менялось.

## Трио завершено — что дальше

err/io/delay готовы: kit имеет три столпа (аллокатор/список/биты) + три инфра-заголовка. Дальше:
- ещё утиль ядра: `jiffies`/времена, `atomic`, `spinlock`/`mutex` — по мере файлов;
- **Lx_kit-рантайм**: timer/workqueue/completion/wait поверх нитей+futex ([[lx-emul-c]]), сведение
  с ioremap/DMA/`request_irq`;
- **driver-model + PCI** → **netdev-подмножество** (чтобы скомпилировался `e1000.h`) → сам драйвер
  `drivers/net/ethernet/intel/e1000/*.c`.

## Связано
- [[lx-err]] (Веха 59) · [[lx-io]] (Веха 60) · [[lx-emul-c]] (буси-udelay уже был там по тикам) ·
  [[lx-kit]] (Lx_kit-рантайм) · [[known-gaps]] · [[todo]]
