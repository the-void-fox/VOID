---
title: Веха 72 — портированный e1000: RX по прерыванию (request_irq ↔ vsys_irq_wait)
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/irq, topic/net, topic/dde-linux, topic/c]
status: done
---

# Веха 72 — RX по ПРЕРЫВАНИЮ: `request_irq` ↔ `vsys_irq_wait`

Веха 71 приняла ARP-ответ **опросом** DD-дескриптора. Эта — тот же приём, но **по реальному
прерыванию карты**: драйвер спит, пока e1000 не поднимет INTx, а не крутит `msleep`. Замыкает
фундамент userspace-драйверов (MMIO+DMA+**IRQ**) на портированном коде. Ядро VOID не менялось.

## Модель: IRQ в кооперативном планировщике Lx_kit

Lx_kit — **один поток** (задача = стек + setjmp/longjmp, [[lx-sched]]); нити под IRQ, как в lx_emul
(Вехи 53–54), тут не годятся. Поэтому доставку IRQ вшили в **idle-путь планировщика** (модель Genode:
EP спит на сигнале, просыпается по IRQ, диспетчеризует):

- `lx_sched_run` в простое: нет готовых задач → сначала таймеры (реальное время до срока); если
  таймеров нет, но зарегистрирован IRQ — зовёт `vsys_irq_wait(cap)` (`SYS_IRQ_WAIT`, [[irq]] Веха 52).
  **Весь процесс блокируется в ядре** до прерывания карты; по возврату зовёт handler в softirq-контексте
  (`sched_current == NULL`) — тот будит ждущую задачу через `complete()`; `continue`. Ни готовых, ни
  таймеров, ни IRQ → работа окончена (`break`).
- `lx_irq_register(irq, cap, handler, dev)` / `lx_irq_unregister` — API в `lx_sched.h`; хранит один
  обработчик на устройство (хватает e1000). Компилируется и в «вычислялке» (Веха 68, без syscall.h):
  никто не регистрирует IRQ → idle-путь его не трогает; сам `vsys_irq_wait` — только под `LX_HAVE_SYSCALL`.

Тонкость (наследие Вехи 52): INTx e1000 — **level-triggered active-low + oneshot-маска**. Пин стартует
замаскированным; первый `vsys_irq_wait` его взводит. Если кадр пришёл ДО взвода — линия остаётся
поднятой (ICR не прочитан), и IOAPIC доставит прерывание сразу при размаскировании. **Кадр не теряется.**

## Что наше (добавка к [[lx-e1000-rx]])

- **`lx_net.c`**: `request_irq` из no-op стал настоящим — при наличии IRQ-cap регистрирует handler в
  планировщике (`lx_irq_register`, каст `irq_handler_t`→`lx_irq_handler_t`: `irqreturn_t` int-совместим).
  `lx_net_set_irq_cap(cap)` — драйвер отдаёт IRQ-cap (start_cap 2). `free_irq`→`lx_irq_unregister`.
- **`drv_e1000.c`**: ISR `e1000_isr` читает `ICR` (er32; R/clr — гасит причину и снимает INTx), при
  RX-причине (`RXT0|RXDMT0|RXO`) зовёт `complete(&g_rx_done)`. В bring-up: `init_completion` +
  `request_irq(0, e1000_isr, …, &g_adapter)` + `ew32(IMS, RXT0|RXDMT0|RXO)`. Приём — **не опрос**:
  `wait_for_completion(&g_rx_done)` в цикле (карта даёт «пороговое» `RXDMT0` ДО кадра — ждём, пока
  реально не появится дескриптор с DD; `reinit_completion` под следующее прерывание). `main()` берёт
  IRQ-cap `vsys_start_cap(2)` → `lx_net_set_irq_cap`.

## Проверка

x86, QEMU q35 `-device e1000,mac=52:54:00:12:34:99` (+ virtio-net ядру):
```
[init] userspace-драйвер lx-e1000-hw P23 (на lx_emul) — выданы MMIO+DMA+IRQ cap
[e1000] BAR0 по MMIO-cap → VA 0x50000000; DMA-cap есть; IRQ-cap есть
[e1000] MAC=52:54:00:12:34:99 STATUS=0x80080783 (link UP) speed=1000
[e1000] TX ARP-запрос «who has 10.0.2.2» → DD=1
[e1000] проснулись по ПРЕРЫВАНИЮ карты: обслужено IRQ=2, ICR=0x00000090
[e1000] ARP-ОТВЕТ от 10.0.2.2: MAC шлюза 52:55:0a:00:02:02
[e1000] Результат: … TX ARP + RX ARP-ответ ПО ПРЕРЫВАНИЮ … — OK
[proc] P23 SYS_EXIT(0) — процесс P23 (все нити группы)
```
`ICR=0x90` = `RXT0`(0x80) | `RXDMT0`(0x10) — реальные RX-причины; `IRQ=2` — обслужено два прерывания
карты (первое пороговое `RXDMT0` до кадра, второе — приход ARP-ответа; цикл ждёт именно кадр с DD).
**Задача продолжилась ТОЛЬКО из-за реального прерывания карты**, не опроса. Регрессии: x86 без
`-device e1000` — драйвер не спавнится, `ping 10.0.2.2` через virtio-net ядра ок; riscv-virt — e1000
нет, не спавнится, vsh+ping ок; `lx_e1000_port` (Веха 68) собирается обе арх. Обе арх: 0 предупреждений.

## Что дальше — штатный ISR/NAPI + мост → ping

- **Штатный путь**: собрать полный `e1000_adapter`+`net_device` через vendored `e1000_probe`→
  `e1000_open`→`e1000_up` ([[lx-pci]] Веха 67, `pci_register_driver`) и завести vendored ISR
  `e1000_intr` + NAPI-poll `e1000_clean`/`e1000_clean_rx_irq` (сейчас handler — наш тонкий, будит
  задачу). NAPI: `napi_schedule_prep`/`__napi_schedule` в `lx_net.c` оживить на задаче-поллере.
- **Мост наружу**: RX→`net-srv`, TX←`net-srv` ([[virtio-net]], Веха 34) → **`ping 10.0.2.2` через
  ПОРТИРОВАННЫЙ e1000** (~Веха 73–74) = точка, где способность хостить неизменённые Linux-драйверы
  ДОКАЗАНА. Дальше драйверы X54C (Atheros/EHCI/wifi) — по требованию тем же конвейером.

## Связано
- [[lx-e1000-rx]] (Веха 71, RX опросом) · [[lx-e1000-tx]] (70) · [[lx-e1000-hw]] (69) · [[lx-e1000-port]] (68) ·
  [[lx-pci]] · [[lx-sched]] · [[lx-wait]] (completion) · [[irq]] · [[userspace-drivers]] · [[virtio-net]] ·
  [[e1000]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[10-lx-kit-runtime]] · [[30-e1000-porting-map]]
