---
title: Веха 69 — портированный e1000 на настоящем QEMU-e1000 через MMIO-cap
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/userspace-drivers, topic/c]
status: done
---

# Веха 69 — неизменённый e1000 читает РЕАЛЬНОЕ железо

Веха 68 показала, что vendored e1000 компилируется/линкуется/исполняется (чистая логика на синтетике).
Эта веха — **тот же неизменённый `e1000_hw.c` работает с НАСТОЯЩЕЙ картой**: спавнится init'ом как
userspace-драйвер (путь Вех 51–54, [[userspace-drivers]]), получает MMIO-cap на регистры реального
QEMU-e1000 и через `er32`/`ew32` (= `readl`/`writel`) сбрасывает карту, читает STATUS и **MAC из
EEPROM реального устройства** — QEMU эмулирует microwire-EEPROM/PHY сам, ничего синтетического.

## Что наше

- **`drv_e1000.c`** — entry userspace-драйвера. `main()`: `vsys_start_cap(0)` → MMIO-cap, `vsys_mmio_map`
  маппит BAR0 (128 КиБ) по VA `0x5000_0000`, кладёт его в `hw->hw_addr`, создаёт задачу и запускает
  планировщик. Работа с картой — в задаче `e1000_bringup` (важно: vendored `e1000_reset_hw` зовёт
  уступающий `msleep` Вехи 63 → нужен кооперативный планировщик Lx_kit под ним). Задача зовёт
  неизменённые `e1000_set_mac_type`/`e1000_reset_hw`/`e1000_init_eeprom_params`/`e1000_read_mac_addr`/
  `e1000_get_speed_and_duplex` и печатает результат, потом `vsys_exit(0)`.
- **`syscall.h` в void-libc** — теперь ставится в `${void-libc}/lib` (был только `lx_emul.h`): драйвер
  берёт `vsys_start_cap`/`vsys_mmio_map`/`vsys_exit` НАПРЯМУЮ, минуя обёртку `lx_emul.h` (её
  `ioremap(cap,len)` конфликтует с `ioremap(phys,size)` из нашего `linux/io.h`).
- **`init.rs`** — spawn-цепочка предпочитает `lx-e1000-hw` (иначе C-драйвер Вехи 54, иначе Rust-каркас);
  права те же — MMIO+DMA+IRQ start-cap. Ядро прочее не тронуто (это оркестрация загрузки).
- Деривация `lx_e1000_drv` → `bin/<arch>/lx-e1000-hw` (`-I${void-libc}/lib` ради `syscall.h`).

## Как это встаёт с ядром

`kmain` (main.rs) предпочитает **virtio-net** и трогает e1000 ТОЛЬКО если virtio-net не поднялся.
Значит в QEMU с ОБОИМИ устройствами (virtio-net + e1000) ядро сидит на virtio-net, а e1000 свободен
для userspace-драйвера — ровно как в [[lx-emul-c]] (Веха 54). `init.rs::probe_e1000` находит карту
(PCI-скан) и спавнит драйвер независимо от выбора сети.

## Проверка

QEMU q35 x86 c `-device e1000,mac=52:54:00:12:34:99` (плюс штатный virtio-net ядру):
```
[init] userspace-драйвер lx-e1000-hw P23 — выданы MMIO+DMA+IRQ cap
[e1000] BAR0 замаплен по MMIO-cap → VA 0x50000000 (128 КиБ)
[e1000] set_mac_type → mac_type=5 (r=0)                    ← e1000_82540
[e1000] STATUS реального e1000 = 0x80080783  (link UP)     ← реальный регистр
Issuing a global reset to MAC                              ← e_dbg vendored-кода
[e1000] reset_hw (сброс настоящей карты) → r=0
[e1000] init_eeprom_params → type=2 word_size=64 (r=0)     ← microwire
[e1000] MAC из EEPROM реального e1000: 52:54:00:12:34:99   ← СОВПАЛ с заданным!
[e1000] speed=1000 duplex=2 (r=0)                          ← из STATUS/PHY
[e1000] Результат: … через MMIO-cap — OK
```
MAC совпал с `mac=…99` → vendored microwire-чтение реально сходило в EEPROM карты. Регрессии:
x86 без `-device e1000` — драйвер не спавнится, `ping` работает; riscv — e1000 нет, драйвер
импортирован, не спавнится, vsh поднимается. Обе арх собирают `lx_e1000_drv`.

## Что дальше — TX/RX

MMIO-путь есть. Дальше:
- **DMA-кольца**: `dma_alloc_coherent` в `lx_net.c` свести с DMA-cap (`vsys_dma_alloc`, start_cap 1);
  e1000 `open` строит кольца TX/RX дескрипторов.
- **IRQ**: `request_irq` → задача на `vsys_irq_wait` (start_cap 2) → ISR/NAPI vendored-кода.
- **Мост наружу**: RX→`net-srv`, TX←`net-srv` ([[virtio-net]], Веха 34) → `ping` через ПОРТИРОВАННЫЙ
  e1000. Затем ethtool/offload. Тот же конвейер потом закроет Atheros/EHCI/wifi X54C.

## Связано
- [[lx-e1000-port]] (Веха 68) · [[lx-pci]] · [[lx-emul-c]] (Веха 54 — тот же cap-путь) · [[e1000]] (родной драйвер) ·
  [[userspace-drivers]] · [[irq]] · [[virtio-net]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
