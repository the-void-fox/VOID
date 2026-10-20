---
title: Веха 53 — lx_emul: первый каркас Linux-API шима (driver-model + threaded-IRQ)
created: 2026-07-01
tags: [project/void, topic/driver, topic/userspace, topic/linux-drivers, topic/lx-emul, topic/capability]
status: done
---

# Веха 53 — lx_emul: драйвер пишется «как в Linux»

Верхний слой пути к хостингу Linux-драйверов (за «длинным хвостом» железа X54C: Atheros-NIC,
EHCI, wifi, GPU — своими силами не написать). Фундамент готов: три примитива по capability дают
драйверу-процессу прямой доступ к железу — MMIO ([[userspace-drivers]], Веха 51), DMA (там же) и
доставка прерывания ([[irq]], Веха 52). Эта веха — **первый каркас Linux-подобного API поверх них**
(в стиле Genode `dde_linux`/`lx_kit`): драйвер пишется так, будто он в ядре Linux, а шим переводит
эти вызовы в syscall'ы VOID. Каркас на **Rust** — доказать driver-model и threaded-IRQ быстро и в
QEMU; C-путь (реальные `.c` из ядра Linux против шима) — следующая веха.

## Что даёт шим (`programs/user/src/lx_emul.rs`)

Тонкий слой, прячущий capability/syscall'ы за именами из мира Linux:

| Linux-API | под капотом (примитив VOID) |
|---|---|
| `ioremap(cap)` · `readl`/`writel`/`readb`/… | `SYS_MMIO_MAP` — окно регистров в свой простор, дальше volatile |
| `dma_alloc_coherent(cap)` → `{cpu, dma}` | `SYS_DMA_ALLOC` — DMA-страница + её физ-адрес (`dma_handle`) |
| `request_irq(cap, handler)` | нить (`SYS_THREAD_SPAWN`), крутящая `SYS_IRQ_WAIT` → handler |
| `kmalloc`/`kzalloc`/`kfree` | ленивая куча процесса (`SYS_MAP`) + свой аллокатор (bump+free-list) |
| `wait_for_completion` / `complete` | futex (`SYS_FUTEX` WAIT/WAKE) |
| `module_init(probe)` (kit) | собрать `Device` из стартовых прав (slot 0/1/2 = mmio/dma/irq), звать `probe` |
| `printk` · `mdelay`/`udelay` | `SYS_WRITE` · монотонный счётчик (`rdtime`/`rdtsc`) |

`Device` — как `struct device`/`pci_dev`, но вместо шинных ресурсов несёт capability'и (их сминтил
init). `probe` не знает ни про syscall'ы, ни про cap — чистый Linux-стиль.

## threaded-oneshot IRQ = ровно наша модель

`request_irq` заводит **нить**, которая спит в `irq_wait` и зовёт handler на каждое прерывание —
это буквально Linux threaded(-oneshot) IRQ: обработчик в контексте нити (может спать, брать замки),
а не в atomic-контексте top-half. И это точно ложится на наш IOAPIC-oneshot ([[irq]]): `irq_wait`
взводит линию перед сном, обработчик ядра снова маскирует — одна доставка на взвод, без шторма.
`SYS_EXIT` рубит всю группу нитей, поэтому нить-IRQ гибнет с драйвером без особой уборки.

## Демо (`bin/lx_e1000`) — e1000 как Linux-драйвер

Тот же e1000, что в `e1000d` (Вехи 51–52), но переписан на шим: `ioremap` регистров, сброс/линк
через `writel`/`readl`, кольцо+буфер через `dma_alloc_coherent`, `request_irq(e1000_irq)` +
`wait_for_completion`. init спавнит именно `lx_e1000` на карту e1000 (сырой `e1000d` оставлен как
образец «до шима»).

Проверено (QEMU, virtio-net ядру + e1000 драйверу):
`[init] userspace-драйвер lx_e1000 P23 (на lx_emul) — выданы MMIO+DMA+IRQ cap` →
`[drv] P23 SYS_MMIO_MAP 0xfebc0000 → 0x50000000` (ioremap) →
`[lx_e1000] драйвер поднят на lx_emul, MAC …:57` →
`[lx_e1000] TX: DD выставлен -- MMIO+DMA через lx_emul РАБОТАЮТ` (dma_alloc_coherent+writel) →
`[lx_e1000] request_irq + жду прерывание (wait_for_completion, опрос выключен)…` →
`[lx_e1000] IRQ! threaded-обработчик разбужен, ICR=0x00000004 -- lx_emul: request_irq РАБОТАЕТ`
(`0x04`=LSC; нить P24 — отдельный слот; после — `SYS_EXIT(0)` рубит обе нити, простор освобождён,
vsh поднялся). riscv — `lx_e1000` компилируется и сеется, но не спавнится (e1000 на QEMU virt нет);
загрузка до vsh без регрессий.

## Что дальше (к реальным Linux-драйверам)

Каркас доказал driver-model + threaded-IRQ. Следующие заходы (уже C, большие):
- **C-`lx_emul`**: вывести драйверные syscall'ы в `void-libc`, минимальные Linux-совместимые
  заголовки, `Lx_kit`-lite (эмуляция scheduler/workqueue/timer — Linux-драйвер ждёт вытесняющий
  контекст), скомпилировать МАЛЕНЬКИЙ C-драйвер против шима.
- **Реальные `.c` из ядра Linux** против шима (полный `dde_linux`): рост API-поверхности до
  сотен функций, порт подсистемы (NIC/USB). Тогда закроется Atheros/EHCI/wifi X54C.
- Без IOMMU dma-cap = доверие «DMA куда угодно» — настоящая изоляция драйверов позже.

## Файлы

- `programs/user/src/lx_emul.rs` — шим (kmalloc/ioremap/dma_alloc_coherent/request_irq/readl-writel/
  Completion/mdelay/module_init+Device).
- `programs/user/src/bin/lx_e1000.rs` — e1000 как Linux-стилевой драйвер поверх шима.
- `programs/user/src/lib.rs` — `pub mod lx_emul;`.
- `kernel/build.rs` + `kernel/src/main.rs` — сев `lx_e1000` в store (`bin/<arch>/lx_e1000`).
- `kernel/src/init.rs` — на карту e1000 спавнится `lx_e1000` (те же MMIO+DMA+IRQ cap).

## Связано
- [[userspace-drivers]] (MMIO+DMA, Веха 51) · [[irq]] (IRQ, Веха 52) · [[e1000]] (та же карта в ядре)
  · [[known-gaps]] · [[todo]]
