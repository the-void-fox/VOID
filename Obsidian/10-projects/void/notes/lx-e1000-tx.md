---
title: Веха 70 — портированный e1000 передаёт кадр (DMA-TX) на реальном QEMU-e1000
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/dma, topic/dde-linux, topic/c]
status: done
---

# Веха 70 — DMA-TX: портированный e1000 выносит кадр наружу

Веха 69 дала MMIO (чтение регистров/MAC реальной карты). Эта — **передачу**: тот же неизменённый
e1000 строит TX-кольцо дескрипторов на НАСТОЯЩЕМ DMA и выносит кадр через реальный QEMU-e1000.
Первое использование DMA-cap ([[userspace-drivers]], Веха 51) портированным Linux-драйвером.

## Что наше

- **`lx_net.c` — реальный DMA** (под `-DLX_HAVE_SYSCALL`): `dma_alloc_coherent` идёт по DMA-cap
  (`vsys_dma_alloc(cap, va)` → device-доступная физ-страница; VA-зона `0x5800_0000`, как DMA_BASE
  lx_emul). Одна страница ≤ 4 КiБ на вызов (TX-кольцо 256×16 = 4 КiБ = ровно страница). Без макроса
  (сборка-«вычислялка» Вехи 68) остаётся куча-версия — DMA там не зовётся. DMA-cap отдаёт драйвер
  (`lx_net_set_dma_cap`, из `vsys_start_cap(1)`).
- **`drv_e1000.c` — TX** (в задаче Lx_kit, после bring-up Вехи 69): минимально заполняем
  `struct e1000_adapter` (hw, pdev, num_tx_queues=1, tx_ring[0].count=`E1000_DEFAULT_TXD`=256) и зовём
  VENDORED `e1000_setup_all_tx_resources` — оно аллоцирует кольцо дескрипторов через наш
  `dma_alloc_coherent` (реальный DMA) + `buffer_info` (vzalloc). TX-движок конфигурируем вручную
  (`ew32` TDBAL/TDBAH/TDLEN/TDH/TDT/TCTL/TIPG + CTRL.SLU) — т.к. vendored `e1000_configure_tx` **static**
  (её зовёт только `e1000_up` в полном open — это следующая веха). Кадр (60 Б broadcast) кладём в
  DMA-буфер, заполняем дескриптор 0 (`buffer_addr`/len+EOP+IFCS+RS), звоним `ew32(TDT, 1)` — карта
  забирает дескриптор, выносит кадр DMA'ом, ставит **DD-бит** и двигает TDH.

## Проверка

x86, QEMU q35 `-device e1000,mac=52:54:00:12:34:99` (+ virtio-net ядру):
```
[e1000] BAR0 по MMIO-cap → VA 0x50000000; DMA-cap есть
[e1000] set_mac_type→mac_type=5; STATUS=0x80080783 (link UP); MAC=52:54:00:12:34:99
[e1000] e1000_setup_all_tx_resources (vendored, DMA-кольцо) → r=0, ring dma=0x13e3000
[e1000] TX: desc0.status=0x01 DD=1, TDH=1 TDT=1 (кадр 60 Б передан по DMA)
[e1000] Результат: … передал кадр на РЕАЛЬНОМ QEMU-e1000 (DMA-cap) — OK
```
`ring dma=0x13e3000` — реальный физ-адрес кольца (от `vsys_dma_alloc`); `DD=1`+`TDH=1` — карта
обработала дескриптор и вынесла кадр наружу. Регрессии: x86 без `-device e1000` — драйвер не
спавнится; riscv — e1000 нет, не спавнится, vsh+ping ок; сборка-«вычислялка» `lx_e1000_port`
(Веха 68, без DMA-cap) собирается обе арх (guard `#ifdef`). Ядро VOID не менялось (init.rs уже
предпочитал `lx-e1000-hw` с Вехи 69).

## Что дальше — RX + IRQ → ping

TX есть. Дальше:
- **RX-кольцо**: `e1000_setup_all_rx_resources` (аналогично), `e1000_alloc_rx_buffers` — skb в
  DMA-память (`dma_map_single` → bounce в DMA-страницу); карта DMA'ит принятые кадры в кольцо.
- **IRQ**: `request_irq` в `lx_net.c` → задача на `vsys_irq_wait` (start_cap 2) → ISR/NAPI-poll
  vendored-кода (`e1000_intr`/`e1000_clean`). Либо опрос RDH/RDT без прерываний для начала.
- **Полный open**: собрать `e1000_adapter`+`net_device` (можно через vendored probe: `pci_register_driver`
  + наш `lx_pci` Вехи 67 → `e1000_probe` → `e1000_open` → `e1000_up`/`e1000_configure_tx/rx`) —
  тогда TX/RX идут штатным vendored-путём (`e1000_xmit_frame`/`e1000_clean_rx_irq`).
- **Мост наружу**: RX→`net-srv`, TX←`net-srv` ([[virtio-net]], Веха 34) → `ping` через ПОРТИРОВАННЫЙ
  e1000. Затем ethtool/offload; тот же конвейер закроет Atheros/EHCI/wifi X54C.

## Связано
- [[lx-e1000-hw]] (Веха 69, MMIO) · [[lx-e1000-port]] (Веха 68) · [[lx-pci]] · [[lx-emul-c]] (Веха 54 — тот же DMA-cap) ·
  [[userspace-drivers]] · [[irq]] · [[virtio-net]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
