---
title: Веха 54 — lx_emul C-путь: C-драйвер против C-шима (кросс-gcc)
created: 2026-07-01
tags: [project/void, topic/driver, topic/userspace, topic/linux-drivers, topic/lx-emul, topic/c]
status: done
---

# Веха 54 — lx_emul на C: настоящий C-драйвер трогает железо

Продолжение [[lx-emul]] (Веха 53, Rust-каркас). Rust-каркас доказал driver-model и threaded-IRQ,
но конечная цель — хостить **реальные `.c` из ядра Linux** (Genode `dde_linux`), а это C. Эта веха —
**первый C-путь**: драйвер на **C**, скомпилированный кросс-gcc против `void-libc` + C-порта шима,
работает на VOID и гонит e1000 Linux-подобным API. Тот самый тулчейн и та самая линковка, что
понадобятся для настоящих Linux-исходников.

## C-шим (`void-libc/lx_emul.{c,h}`, вкомпилен в `libvoid.a`)

`lx_emul.h` — **чистый Linux-API** (не тянет `syscall.h`): драйвер не видит ни cap, ни ecall.
Имена — как в ядре Linux, где не сталкиваются с newlib:

| Linux-API (C) | под капотом |
|---|---|
| `ioremap` · `readl`/`writel`/`readb`/`writeq` (inline volatile) | `SYS_MMIO_MAP` |
| `dma_alloc_coherent → struct lx_dma {cpu, dma}` | `SYS_DMA_ALLOC` |
| `request_irq(cap, handler)` | нить (`SYS_THREAD_SPAWN`), крутящая `SYS_IRQ_WAIT` |
| `kmalloc`/`kzalloc`/`kfree` | куча процесса (newlib `malloc`, зовётся из main) |
| `init_completion`/`complete`/`wait_for_completion` | futex (`SYS_FUTEX`) |
| `printk(fmt, …)` | **newlib `vsnprintf`** → `SYS_WRITE` (форматный, как настоящий printk) |
| `lx_module_init(probe)` + `struct lx_device` | собрать cap из стартовых прав (0/1/2), звать probe |

`syscall.h` дорос драйверными обёртками (`vsys_mmio_map`/`vsys_dma_alloc`/`vsys_irq_wait`/
`vsys_thread_spawn`/`vsys_thread_exit`/`vsys_futex_*`) поверх общего `vsys(n,a0..a6)`. `lx_emul.o`
лежит в `libvoid.a`; hello/bzip2 его функций не зовут — .o не линкуется (статический архив).

## Демо (`programs/lx-cdriver/lx_e1000.c`) — тот же e1000, но на C

Порт Rust-драйвера [[lx-emul]] на C: `probe` делает `ioremap`, сброс/линк через `writel`/`readl`,
кольцо+буфер через `dma_alloc_coherent`, TX (DD), `request_irq(e1000_irq)` + `wait_for_completion`.
Кириллица в `printk` — свободно (C-литералы держат UTF-8, в отличие от Rust byte-strings). Threaded-
обработчик читает ICR и сигналит `complete` — как threaded-oneshot IRQ Linux.

## Сборка и доставка (C-мир: nix-build → мост → диск)

C-программы **не** встроены в образ ядра (в отличие от Rust `programs/user`) — собираются на хосте
и едут на диск мостом ([[store-bridge]]), как hello/bzip2 ([[nixpkgs-cross]]):

```
nix-build nix -A x86_64.lx_e1000    # кросс-gcc + void-libc + specs → VOID-ELF
nix-build nix -A riscv64.lx_e1000
void-store-import void-disk.img put result/bin/lx_e1000 bin/<arch>/lx_e1000_c
```

`nix/default.nix`: деривация `lx_e1000` (свой C-исходник, `$CC -B… -specs=void.specs -I… -static`);
глю `void-libc` пересобрана с `lx_emul.o` в `libvoid.a` и ставит `lx_emul.h`. Тулчейн pkgsCross —
**оффлайн** (уже в локальном сторе; сети в песочнице нет). `init.rs`: на карту e1000 спавнится
`lx_e1000_c`, **если импортирован** (`bin/<arch>/lx_e1000_c` в store), иначе Rust-каркас `lx_e1000`
(Веха 53) — чистый ребилд ядра без импорта всё ещё поднимает Rust-драйвер. C-ELF живёт только на
диске (`void-disk.img` в .gitignore) — воспроизводится командами выше.

Проверено (QEMU x86, virtio-net ядру + e1000 драйверу):
`[init] userspace-драйвер lx_e1000_c P23 (на lx_emul) — выданы MMIO+DMA+IRQ cap` →
`[drv] P23 SYS_MMIO_MAP 0xfebc0000 → 0x50000000` (ioremap) →
`[lx_e1000-c] драйвер поднят на lx_emul (C), MAC …:57` (printk %02x) →
`[lx_e1000-c] TX: DD выставлен -- MMIO+DMA через lx_emul(C) РАБОТАЮТ` →
`[lx_e1000-c] IRQ! threaded-обработчик разбужен, ICR=0x00000004 -- lx_emul(C): request_irq РАБОТАЕТ`
(нить C через SYS_THREAD_SPAWN; после — SYS_EXIT рубит обе нити, vsh поднялся). riscv — C-драйвер
компилируется и импортирован, но не спавнится (e1000 на QEMU virt нет); загрузка без регрессий.
Обе арх: 0 предупреждений.

## Что дальше (к реальным Linux-драйверам)

C-тулчейн-путь проверен на своём C-драйвере. Дальше — **реальные `.c` из ядра Linux** против шима
(полный `dde_linux`): рост API-поверхности до сотен функций, `Lx_kit` (эмуляция scheduler/workqueue/
timer — Linux-драйвер ждёт вытесняющий контекст), минимальные Linux-совместимые заголовки, потом
порт подсистемы (NIC/USB). Тогда закроется Atheros/EHCI/wifi X54C. Без IOMMU dma-cap = доверие.

## Файлы

- `programs/void-libc/lx_emul.h` — чистый Linux-API (без syscall.h); `lx_emul.c` — реализация (в libvoid.a).
- `programs/void-libc/syscall.h` — драйверные обёртки (`vsys_mmio_map`/`dma_alloc`/`irq_wait`/`thread_spawn`/`futex`).
- `programs/lx-cdriver/lx_e1000.c` — e1000 как Linux-стилевой C-драйвер.
- `nix/default.nix` — `lx_emul.o` в libvoid.a + деривация сборки C-драйвера (`<arch>.lx_e1000`).
- `kernel/src/init.rs` — на e1000 спавнится `lx_e1000_c` (если импортирован), иначе Rust `lx_e1000`.

## Связано
- [[lx-emul]] (Rust-каркас, Веха 53) · [[nixpkgs-cross]] (C-мир, тот же тулчейн) · [[store-bridge]]
  (доставка) · [[userspace-drivers]] · [[irq]] · [[known-gaps]] · [[todo]]
