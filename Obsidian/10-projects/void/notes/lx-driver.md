---
title: Веха 66 — driver-model + module_init Lx_kit
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 66 — driver-model + module_init: переход к самому драйверу

Рантайм-примитивы Lx_kit готовы (Вехи 56–65). С этой вехи **начинается переход к самому драйверу**:
метод меняется на линкер-ориентированный ([[20-linker-driven-workflow]]) — но сперва нужны две
опоры, на которых стоит любой Linux-драйвер: **точка входа модуля** и **driver-model** (регистрация
устройство↔драйвер, вызов `.probe`). На них дальше встанет PCI, а на PCI — e1000.

## Что наше

- **`linux/module.h`** (шим) — `module_init(fn)`/`module_exit(fn)` перенаправлены в функции с
  ФИКСИРОВАННЫМИ именами `lx_module_init`/`lx_module_exit` (в бинаре ровно один драйвер = один
  module_init; их зовёт наша glue-точка входа — приём из Genode dde_linux). `THIS_MODULE`, `__init`/
  `__exit` — пустые; `MODULE_LICENSE`/`MODULE_DEVICE_TABLE`/`module_param`/`MODULE_PARM_DESC`/… — в
  пустоту (параметры остаются глобалами с дефолтами). `EXPORT_SYMBOL` — из `linux/export.h`.
- **`linux/device.h`** (шим) — `struct device`/`struct device_driver`/`struct bus_type` (с функцией
  `match`); `dev_get/set_drvdata`, `dev_name`, `dev_info`/`dev_err`/`dev_warn`/… → printk. Регистрация
  (`bus_register`/`driver_register`/`device_register`/`device_add` + `*_unregister`/`device_del`).
- **`lx_kit.c`** (тела) — реестр драйверов и устройств (односвязные списки в приватных `lx_next`);
  `lx_try_bind`: `dev->bus == drv->bus && bus->match(dev,drv)` → `dev->driver = drv` → `drv->probe(dev)`
  (при отказе, в т.ч. `-EPROBE_DEFER`, — откат). `driver_register` пробует связать со всеми
  известными устройствами, `device_register` — со всеми драйверами. `*_unregister` зовёт `.remove`.
  Упрощённый `drivers/base/dd.c`.
- **`main_driver.c`** (харнесс) — фейковые шина/драйвер/устройство: `device_register` без драйвера
  (probe НЕ вызван) → `module_init`→`lx_module_init`→`driver_register` → match → `.probe` (drvdata) →
  `device_unregister` → `.remove`.

## Сборка/запуск

`nix-build nix -A <arch>.lx_driver` → мост `put … bin/<arch>/lx-driver` → `run bin/lx-driver`
(деривация `lx_driver`: `main_driver.c lx_kit.c`). Планировщик тут не нужен (прямой сценарий).

Проверено (QEMU, **обе арх идентично**): `device_register (драйвера нет): probe=0`; `module_init →
driver_register → probe: 'fakedev0' ← 'fakedrv'`; `probe=1, связан с 'fakedrv', drvdata=0xabcd`;
`device_unregister → remove`; `Результат: probe=1 remove=1 drvdata=0xabcd OK`. `-Wall -Wextra
-Wclobbered -Wcomment` чисто; регрессий у прочих `lx_*` нет; ядро не менялось.

Грабля (повтор): в блок-комментарии `MODULE_*/module_param` — `*/` рвёт комментарий; переформулировал
«MODULE_… и module_param». Проверять сборку с `-Wcomment`.

## Что дальше (к e1000)

Опоры есть. Дальше по карте reference/dde-linux/[[30-e1000-porting-map]]:
- **PCI** (`linux/pci.h`): `pci_dev` встраивает `struct device`, `pci_driver` — `device_driver`,
  `pci_register_driver` крутит ту же связку match/probe поверх нашего PCI-обхода + MMIO/DMA-cap
  (Вехи 51–52, [[userspace-drivers]], [[irq]]). Здесь же — **первый реальный `.c`** и генератор
  заглушек (компилировать, гасить неопределённые символы).
- **netdev-подмножество** (`netdevice.h`/`skbuff.h`/`etherdevice.h`/`dma-mapping.h`) → `e1000_hw.c`
  → `e1000_main.c`.

## Связано
- [[lx-work]] (Веха 65) · [[lx-kit]] (аллокатор) · [[lx-emul-c]] (Rust/C-каркас драйвер-модели — свести) ·
  [[userspace-drivers]] · [[irq]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
