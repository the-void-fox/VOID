---
title: Веха 71 — портированный e1000: TX+RX (ARP round-trip) на реальном QEMU-e1000
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/dma, topic/net, topic/dde-linux, topic/c]
status: done
---

# Веха 71 — RX: портированный e1000 принимает кадр (ARP round-trip)

Веха 70 дала TX (вынос кадра по DMA). Эта — **приём**: тот же неизменённый e1000 строит RX-кольцо на
DMA, шлёт ARP-запрос шлюзу и **принимает ARP-ответ** с реального QEMU-e1000. Двунаправленный реальный
сетевой обмен через портированный драйвер — без IRQ (опрос DD-дескриптора) и без моста к net-srv.

## Что наше (добавка к [[lx-e1000-tx]])

- **RX-кольцо** (в `drv_e1000.c`, задача Lx_kit): `adapter.num_rx_queues=1`, `rx_ring[0].count=
  E1000_DEFAULT_RXD`=256 → VENDORED `e1000_setup_all_rx_resources` строит кольцо дескрипторов на
  реальном DMA. RX-буферы — по DMA-странице на дескриптор (`RX_AVAIL`=4), их физ-адреса в
  `rx_desc[i].buffer_addr`. RX-движок конфигурим вручную (`e1000_configure_rx` static): `RDBAL/RDBAH/
  RDLEN/RDH=0/RDT=RX_AVAIL` + `RCTL = EN|BAM|UPE|MPE|SECRC` (промиск — примем и уникаст-ответ; SECRC
  срезает CRC). Приёмный адрес `RAL0/RAH0` = наш MAC (+`RAH_AV`).
- **ARP round-trip**: строим ARP-запрос «who has 10.0.2.2» (Ethernet+ARP, 60 Б, broadcast, spa=10.0.2.15),
  TX через дескриптор (Веха 70) → `ew32(TDT,1)`; ждём DD. Затем опрашиваем RX-дескрипторы на DD (до 4 с,
  `msleep(1)`), при DD парсим кадр: ethertype 0x0806 + ARP oper=2 → извлекаем MAC шлюза (sha) и spa.

## Проверка

x86, QEMU q35 `-device e1000,mac=52:54:00:12:34:99` (+ virtio-net ядру):
```
[e1000] MAC=52:54:00:12:34:99 STATUS=0x80080783 (link UP) speed=1000
[e1000] vendored setup TX/RX-колец на DMA → r=0 (tx dma=0x13e3000, rx dma=0x13e6000)
[e1000] TX ARP-запрос «who has 10.0.2.2» → DD=1
[e1000] RX: дескриптор 0 DD, 64 Б, ethertype=0806
[e1000] ARP-ОТВЕТ от 10.0.2.2: MAC шлюза 52:55:0a:00:02:02
[e1000] Результат: … TX ARP + RX ARP-ответ … (DMA-cap) — OK
```
`MAC шлюза 52:55:0a:00:02:02` — штатный SLIRP-шлюз (52:55 + IP 0a:00:02:02): ответ реально пришёл с
QEMU-SLIRP через RX-кольцо. Регрессии: x86 без `-device e1000` — драйвер не спавнится; riscv-virt —
не спавнится, vsh+ping ок; `lx_e1000_port` (Веха 68) собирается обе арх. Ядро VOID не менялось.

## Что дальше — IRQ + net-srv → ping

TX+RX опросом есть. Дальше:
- **IRQ вместо опроса**: `request_irq` в `lx_net.c` → задача на `vsys_irq_wait` (start_cap 2, Веха 52) →
  vendored ISR `e1000_intr` + NAPI-poll `e1000_clean`/`e1000_clean_rx_irq`. Для ШТАТНОГО пути собрать
  полный `e1000_adapter`+`net_device` через vendored `e1000_probe`→`e1000_open`→`e1000_up`
  (`pci_register_driver` + наш [[lx-pci]] Вехи 67).
- **Мост наружу**: RX→`net-srv`, TX←`net-srv` ([[virtio-net]], Веха 34) — драйвер отдаёт принятые кадры
  net-srv-серверу (ARP/IPv4/ICMP) и шлёт его кадры → **`ping 10.0.2.2` через ПОРТИРОВАННЫЙ e1000**.
- Затем ethtool/offload; тот же конвейер закроет Atheros/EHCI/wifi X54C.

## Связано
- [[lx-e1000-tx]] (Веха 70, TX) · [[lx-e1000-hw]] (Веха 69, MMIO) · [[lx-e1000-port]] (Веха 68) · [[lx-pci]] ·
  [[lx-emul-c]] · [[userspace-drivers]] · [[irq]] · [[virtio-net]] · [[e1000]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
