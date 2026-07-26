---
title: Веха 52 — доставка прерываний в userspace (IRQ по capability)
created: 2026-07-01
tags: [project/void, topic/driver, topic/userspace, topic/interrupt, topic/ioapic, topic/capability, topic/linux-drivers]
status: done
---

# Веха 52 — прерывания в userspace: последний кусок фундамента

Второй (и последний) механизм фундамента [[userspace-drivers]] под хостинг Linux-драйверов: без
доставки прерываний драйвер вынужден **опрашивать** железо (наш e1000d из Вехи 51 крутил `for` до
бита DD). Interrupt-драйверы и Linux `request_irq` без этого не работают. Теперь userspace-драйвер
может **уснуть до прерывания своего устройства** и проснуться от него — процессор при этом свободен.

## Механизм (ядро)

- **IRQ по capability.** Новая эфемерная цель `Target::Irq { vector }` — право «ждать это
  прерывание». Минтится на загрузке после того, как init замаршрутизировал IRQ устройства на наш
  вектор. `cap::irq(cap, READ)` резолвит её.
- **Вектор `VEC_USERDRV = 35`.** Отдельный IDT-вектор (trap-stub 35 + гейт) под прерывания
  userspace-драйверов. Обработчик НЕ берёт замок таблицы процессов (иначе дедлок с прерванным
  контекстом) — только выставляет атомарный флаг `USERDRV_IRQ_PENDING` и делает EOI.
- **`SYS_IRQ_WAIT(irq_cap)` (33).** Драйвер уходит в новое состояние `State::IrqWait`, планировщик
  даёт ход другим. Кадр продвигается сразу (при пробуждении вернётся 0). Разбудит `drain_userdrv_irq`
  — он снимает флаг и переводит всех ждущих в `Runnable`. Зовётся на каждом trap'е из U (`resume`)
  и в `wait_stdin`, поэтому уже пришедший IRQ ловится без потери. `wait_stdin` держит сессию живой,
  пока кто-то в `IrqWait` (`any_irq_waiting`), и просыпается от `USERDRV_IRQ_PENDING` наравне с вводом.

## IOAPIC: level-triggered active-low + oneshot-маска

Главная тонкость. QEMU-шная e1000 **не имеет MSI** (пустой список PCI-cap, `pin=1`, INTx на линии
11) — прерывание идёт по разделяемой PCI INTx через IOAPIC. Два открытия при отладке:

1. **Полярность/триггер.** PCI INTx — **level-triggered, active-low**. Изначальный `route`
   (edge, active-high) ловил бы «не тот» фронт (снятие линии, а не подъём) — прерывание не
   доставлялось вовсе. Нужен `route_level_low` (RTE bits: polarity 1<<13, trigger 1<<15). Слот→PIRQ
   на q35 дал бы ACPI `_PRT` (не парсим) — маршрутизируем все четыре PCI-линии GSI 16..23 на вектор.
2. **Шторм.** На x86 прерывания устройств НЕ маскируются на время сессии (обслуживаются диспетчером
   из любого кольца). Карта держит level-INTx, пока драйвер не прочитает ICR — а он читает лишь
   ПОСЛЕ пробуждения. Без защиты IOAPIC переотправлял бы прерывание штормом. Решение — **oneshot**:
   пины стартуют замаскированными (RTE bit 16); `SYS_IRQ_WAIT` их **взводит** (`userdrv_irq_arm`
   → размаскировать) прямо перед сном; обработчик VEC_USERDRV по факту прерывания **снова маскирует**.
   Ровно одна доставка на взвод — как threaded-oneshot IRQ в Linux. Гонки потерянного пробуждения
   нет: если причина уже висит на карте, размаскировка в `SYS_IRQ_WAIT` доставит прерывание сразу.

## Демо (`bin/e1000d`, продолжение Вехи 51)

После polled-TX драйвер получает IRQ-cap (3-е стартовое право), размаскирует причину в `IMS`,
инициирует её через `ICS` (программный self-test — не ждём линка/пакета), зовёт `SYS_IRQ_WAIT`
(опрос выключен) и просыпается от **реального прерывания карты**, разбирая причину из `ICR`.

Проверено (QEMU, virtio-net ядру + e1000 драйверу):
`[e1000d] жду прерывание карты (SYS_IRQ_WAIT, опрос выключен)…` →
`[e1000d] IRQ! проснулся от прерывания карты, ICR=0x00000004 — прерывания в userspace РАБОТАЮТ`
(`0x04` = LSC, ровно та причина, что дёрнули через ICS). riscv — `userdrv_irq_arm`/`e1000_irq_setup`
заглушки (таких драйверов там пока нет), загрузка до vsh без регрессий.

## Что дальше (к Linux-драйверам)

Фундамент [[userspace-drivers]] закрыт целиком: **MMIO + DMA (Веха 51) + IRQ (Веха 52)**. Остаётся
верхний слой — **Linux-API шим (`lx_emul`)**: kmalloc/ioremap/DMA/`request_irq`/driver-model поверх
этих трёх примитивов (Genode `dde_linux`-стиль), затем порт конкретной подсистемы. Большой заход.

## Файлы

- `kernel/src/cap.rs` — `Target::Irq { vector }`, `cap::irq`.
- `kernel/src/proc.rs` — `State::IrqWait`, `SYS_IRQ_WAIT` (33), `USERDRV_IRQ_PENDING`,
  `on_userdrv_irq`/`drain_userdrv_irq`/`any_irq_waiting`, взвод в `wait_stdin`/`resume`.
- `kernel/src/arch/x86_64/ioapic.rs` — `route_level_low`, `set_userdrv_masked` (oneshot-маска).
- `kernel/src/arch/x86_64/{trap.rs,trap_entry.s}` — `VEC_USERDRV=35`, stub, маска в обработчике.
- `kernel/src/arch/x86_64/pci.rs` — `e1000_irq_setup` (снять Interrupt Disable, GSI 16..23 level-low).
- `kernel/src/arch/x86_64/mod.rs` + `riscv64/mod.rs` — `userdrv_irq_arm` (взвод / заглушка).
- `kernel/src/init.rs` — IRQ-cap 3-м стартовым правом e1000d.
- `programs/user/src/lib.rs` — `irq_wait`; `bin/e1000d.rs` — IRQ-демо поверх polled-TX.

## Связано
- [[userspace-drivers]] (слой 1: MMIO+DMA) · [[e1000]] (та же карта в ядре) · [[known-gaps]] · [[todo]]
