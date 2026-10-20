---
title: Веха 67 — шина PCI поверх driver-model
created: 2026-07-01
tags: [project/void, topic/driver, topic/pci, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 67 — PCI: последняя опора под сам драйвер

Две опоры любого Linux-драйвера завёл [[lx-driver]] (Веха 66): точка входа модуля и driver-model.
**PCI — третья и последняя опора под e1000**: он висит на шине PCI, опознаётся по `id_table`
(vendor/device), а окно регистров берёт из BAR'а. Ключ: PCI НЕ переизобретает связывание —
`pci_dev` встраивает `struct device`, `pci_driver` — `struct device_driver`, а `pci_register_driver`
крутит ту же связку `driver_register`→match→`.probe` Вехи 66. Различие лишь в правиле match: не имя,
а перебор `id_table`.

## Что наше

- **`linux/pci.h`** (шим) — `struct pci_dev` (встраивает `struct device`; поля `vendor`/`device`/
  `subsystem_*`/`revision`/`irq`; BAR'ы `struct resource resource[PCI_STD_NUM_BARS]`; конфиг
  `lx_config[64]`; приватные `lx_id`/`lx_driver`), `struct pci_driver` (встраивает `struct
  device_driver`; `id_table`, `probe(pdev, id)`, `remove`, `shutdown`, `err_handler`). Макросы
  `PCI_DEVICE`/`PCI_VDEVICE`/`PCI_ANY_ID`, `PCI_VENDOR_ID_INTEL`(0x8086)/`_VMWARE`, `PCI_STD_NUM_BARS`,
  смещения конфига (`PCI_VENDOR_ID`/`PCI_COMMAND`/…), состояния питания (`PCI_D0..D3cold`), типы AER
  (`pci_channel_state_t`/`pci_ers_result_t`/`pci_error_handlers`) — всё, что трогает e1000. Аксессоры
  `pci_resource_start/end/len/flags` и `pci_get/set_drvdata`/`pci_name` — inline; `to_pci_dev`/
  `to_pci_driver` — спуск от встроенной базы через `offsetof`.
- **`linux/ioport.h`** (шим) — `struct resource` (диапазон адресов: start/end/name/flags) + `IORESOURCE_*`
  + `resource_size`. У PCI это BAR'ы; менеджера ресурсов не держим.
- **`lx_kit.c`** (тела) — `pci_bus_type` с `match = lx_pci_bus_match` (перебирает `id_table` драйвера,
  сверяет vendor/device/subsystem с учётом `PCI_ANY_ID`; при совпадении прячет `pdev->lx_id`/
  `lx_driver`). `pci_register_driver` проецирует `drv->name/bus/probe/remove` на встроенный
  `device_driver` (probe/remove — мосты `lx_pci_dev_probe`/`lx_pci_dev_remove`, разворачивающие
  базовый вызов в `pci_driver->probe(pdev, id)`) и зовёт `driver_register` Вехи 66. `lx_pci_register_device`
  — роль перечислителя ядра (ставит `dev.bus=&pci_bus_type` + `device_register`). `pci_enable_device`/
  `set_master`/`set_mwi` — выставляют биты в конфиг-слове `PCI_COMMAND`; `pci_select_bars` — реальная
  битовая маска BAR'ов по флагам; конфиг чтение/запись — LE над `lx_config[]`; `pci_ioremap_bar` —
  `ioremap` (identity) над стартом BAR; `save_state`/`set_power_state`/`enable_wake` — учётные.
- **`main_pci.c`** (харнесс) — синтетический **8086:100E** (82540EM, реальный e1000) с BAR0-окном на
  буфер-«регистры»: `lx_pci_register_device` (драйвера нет → probe=0) → `module_init`→
  `pci_register_driver` → match по `id_table` → probe идёт «как e1000» (enable_device → set_master →
  select_bars(MEM) → request_selected_regions → `ioremap_bar` → `readl` сигнатуры + чтение vendor/device
  из конфига + drvdata) → `pci_unregister_driver` → remove.

## Сборка/запуск

`nix-build nix -A <arch>.lx_pci` → мост `put … bin/<arch>/lx-pci` → `run bin/lx-pci`
(деривация `lx_pci`: `main_pci.c lx_kit.c`).

Проверено (QEMU, **обе арх идентично**): `lx_pci_register_device (драйвера нет): probe=0`;
`module_init → pci_register_driver('e1000-demo')`; `probe: 0000:00:03.0 vendor=8086 device=100e`;
`select_bars(MEM)=0x1  COMMAND после enable/master: 0x0007` (IO|MEMORY|MASTER); `ioremap_bar(0) →
readl(+0) = 0xe1000ba5`; `remove drvdata=0x1000e`; `Результат: probe=1 remove=1 vendor=8086
device=100e reg=0xe1000ba5 OK`. `-Wall -Wextra -Wclobbered -Wcomment` чисто (обе кросс-арки, включая
арх-ветки `arch_execute`); регрессий у прочих `lx_*` нет; ядро не менялось.

## Что дальше (к e1000)

**Все опоры готовы** (kit-примитивы + driver-model + PCI). Дальше по [[30-e1000-porting-map]] —
собственно драйвер, линкер-ориентированно ([[20-linker-driven-workflow]]):
- **Первый реальный `.c` и генератор заглушек**: компилировать файл ядра, гасить неопределённые
  символы (`generated_dummies` в стиле Genode).
- **netdev-подмножество** (`netdevice.h`/`skbuff.h`/`etherdevice.h`/`dma-mapping.h`), чтобы
  скомпилировался `e1000.h` → `e1000_hw.c` → `e1000_main.c`. `pci_ioremap_bar`/BAR-старт сведём с
  реальным окном по MMIO-cap VOID (Вехи 51/54, [[userspace-drivers]]), IRQ — по [[irq]].

## Связано
- [[lx-driver]] (Веха 66, driver-model — PCI стоит на ней) · [[lx-kit]] · [[lx-emul-c]] ·
  [[userspace-drivers]] · [[irq]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
