---
title: Веха 73 — PING через портированный e1000 (ARP + ICMP, DMA + IRQ)
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/net, topic/icmp, topic/dde-linux, topic/c]
status: done
---

# Веха 73 — PING шлюза через ПОРТИРОВАННЫЙ e1000

**Точка доказательства.** Неизменённый Linux-драйвер e1000 (`e1000_hw.c`/`e1000_main.c`/`e1000_param.c`,
Linux 6.18.7 verbatim) на VOID делает **полный IP round-trip до живого шлюза**: ARP узнаёт MAC шлюза →
ICMP echo request → приём echo reply — всё на реальном DMA и **по прерыванию** карты. Способность
VOID хостить неизменённые Linux-драйверы (микроядро, userspace, capability-изоляция) — **доказана**.

## Что наше (добавка к [[lx-e1000-irq]])

Всё в `drv_e1000.c` (bring-up остаётся задачей Lx_kit; vendored код гоняет карту/DMA/reset/PHY):
- **Хелперы** (сам L3/L4 собираем — стек это net-srv, мост к нему дальше): `inet_csum` (RFC 1071),
  `build_icmp_echo` (Ethernet + IPv4 + ICMP echo + 32 Б payload, обе контрольные суммы),
  `e1000_tx(hw, tx, idx, dma, len)` (дескриптор TX-кольца + `ew32(TDT)` + ждать DD),
  `e1000_wait_rx(rx, &from)` (ждать следующий DD-дескриптор ПО ПРЕРЫВАНИЮ, сканируя с `from` — цикл
  `wait_for_completion`/`reinit`, пропуская пороговые прерывания без кадра).
- **Стадии**: (1–2) bring-up + кольца + IRQ (Вехи 69–72); (3) **ARP**: TX запрос → `e1000_wait_rx` →
  из ответа берём `sha` = MAC шлюза; (4) **PING**: `build_icmp_echo` к 10.0.2.2 → `e1000_tx` (desc 1) →
  ждём echo reply, **пропуская чужие кадры** (проверяем ethertype IPv4 + proto ICMP + type=0 +
  src=шлюз), пока не кончатся дескрипторы; печатаем id/seq/ttl.

Ключевое: TX и приём — **тот же тракт**, что подняли Вехи 70–72 (кольца дескрипторов vendored-кодом на
DMA-cap, приём через `vsys_irq_wait`+ISR+completion). Веха 73 добавляет только сборку/разбор пакетов.

## Проверка

x86, QEMU q35 `-device e1000,mac=52:54:00:12:34:99` (+ virtio-net ядру):
```
[e1000] MAC=52:54:00:12:34:99 STATUS=0x80080783 (link UP) speed=1000
[e1000] TX ARP-запрос «who has 10.0.2.2» → DD=1
[e1000] ARP-ОТВЕТ: 10.0.2.2 = 52:55:0a:00:02:02 (обслужено IRQ=2, ICR=0x00000090)
[e1000] TX ICMP echo request → 10.0.2.2 (74 Б) → DD=1
[e1000] ICMP ECHO REPLY от 10.0.2.2: id=0x1234 seq=1 ttl=255 (обслужено IRQ=3)
[e1000] Результат: PING 10.0.2.2 через НЕИЗМЕНЁННЫЙ e1000 (DMA+IRQ, ARP+ICMP) — OK
```
Echo reply реально пришёл от SLIRP-шлюза (`ttl=255`, `id/seq` совпали) через RX-кольцо по прерыванию.
Регрессии: x86 без `-device e1000` — драйвер не спавнится, `ping 10.0.2.2` через virtio-net ядра
(7143 мкс) ок; riscv-virt e1000 нет — не спавнится, vsh+ping ок (941 мкс); сборка обеих арх, 0
предупреждений. Ядро VOID не менялось.

## Значение и что дальше

**Конвейер хостинга неизменённых Linux-драйверов доказан** (компиляция → исполнение → MMIO → DMA-TX →
DMA-RX → IRQ → **ping**). Дальше — **по требованию** ([[known-gaps]], решение обеих сторон): драйверы
конкретного железа X54C (Atheros/Realtek NIC, EHCI USB, wifi ath9k/mac80211 — гора, GPU DRM+Mesa —
бо́льшая гора) портируем тем же конвейером, когда нужен живой чип, а не линейным списком. Опциональная
полировка e1000 (НЕ обязательна для доказательства): штатный `e1000_probe`→`e1000_open`→`e1000_up`
(vendored ISR `e1000_intr` + NAPI `e1000_clean`) и **мост RX/TX ↔ net-srv** ([[virtio-net]]) — чтобы
`vsh ping` шёл через портированный драйвер как общий сетевой backend. Следующая крупная фаза роадмапа —
**vvsh** ([[0006-vvsh-lisp-config-shell]]).

## Связано
- [[lx-e1000-irq]] (72, IRQ) · [[lx-e1000-rx]] (71) · [[lx-e1000-tx]] (70) · [[lx-e1000-hw]] (69) ·
  [[lx-e1000-port]] (68) · [[lx-pci]] · [[lx-sched]] · [[userspace-drivers]] · [[irq]] · [[virtio-net]] ·
  [[e1000]] · [[known-gaps]] · [[todo]] · [[target-system]] (зачем: host-agnostic boot)
- Референс: reference/dde-linux/[[30-e1000-porting-map]]
