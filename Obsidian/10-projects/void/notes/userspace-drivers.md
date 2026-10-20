---
title: Веха 51 — фундамент userspace-драйверов (MMIO+DMA по capability)
created: 2026-07-01
tags: [project/void, topic/driver, topic/userspace, topic/mmio, topic/dma, topic/capability, topic/linux-drivers]
status: done
---

# Веха 51 — userspace-драйверы: фундамент под хостинг Linux-драйверов

Первый шаг к запуску Linux-драйверов (владелец хочет закрыть «длинный хвост» железа X54C:
Atheros-NIC, EHCI, wifi, GPU — своими силами их не написать). Хостинг Linux-драйвера требует
трёх слоёв: (1) доступ к железу из userspace, (2) Linux-API шим (`lx_emul`), (3) склейка. Эта
веха — **слой 1**, фундамент под всё: без него ни один хостируемый (да и вообще userspace)
драйвер не тронет устройство. Заодно это микроядерно правильно — драйверам место в userspace.

## Три механизма (ядро)

- **MMIO по capability.** Новая цель `Target::Mmio { base, len }` (окно регистров устройства,
  физ. база+длина) — минтится на загрузке после PCI-поиска. `SYS_MMIO_MAP(cap, va)` маппит это
  окно в адресный простор процесса (U|R|W), и драйвер читает/пишет регистры железа НАПРЯМУЮ
  volatile'ом по `va` — без syscall'ов на каждый регистр.
- **DMA-память с физ-адресом.** Цель `Target::Dma` (право выделять DMA). `SYS_DMA_ALLOC(cap, va)`
  выделяет фрейм, маппит по `va` и **возвращает его ФИЗИЧЕСКИЙ адрес** — им драйвер программирует
  DMA устройства (кольца/буферы). Без IOMMU это доверенное право (DMA куда угодно) — только драйверам.
- **Снос простора не трогает MMIO.** Тонкость с [[frame-freeing]]: при выходе драйвера
  `free_address_space` обходит его таблицы, а там есть листья, указывающие на MMIO УСТРОЙСТВА
  (не RAM). `frame::is_ram(pa)` отсеивает их — иначе адрес железа (напр. `0xfebc0000`) попал бы в
  список свободных фреймов и был бы выдан как страница RAM. RAM-листья (обычные, DMA, таблицы)
  освобождаются как обычно.

Обе цели эфемерны (не персистятся: устройство ищется заново на каждой загрузке). init получил
токены конфига `mmio:<dev>` (PCI-поиск → Mmio-cap) и `dma`; `sys::mmio_map`/`dma_alloc` — в void_user.

## Демо — e1000 В USERSPACE (`bin/e1000d`)

Драйвер Intel e1000 как ОБЫЧНЫЙ процесс (не в ядре!): маппит BAR по MMIO-cap, выделяет DMA
TX-кольцо+буфер, сбрасывает карту, читает MAC (MMIO), отправляет broadcast-кадр и ждёт, пока
**карта выставит бит DD** (Descriptor Done) — значит устройство САМО прочитало userspace-DMA
дескриптор и буфер. Чтобы не драться с ядром за карту: сеть ядра — на virtio-net (первой), а
e1000 отдана драйверу (init спавнит `e1000d`, если e1000 свободна).

Проверено (QEMU, virtio-net ядру + e1000 драйверу): `[drv] SYS_MMIO_MAP 0xfebc0000 (32 стр.) →
0x50000000`; `[e1000d] userspace-драйвер поднят, MAC …:57`; `[e1000d] TX: карта выставила DD —
MMIO+DMA из userspace РАБОТАЮТ`. e1000d чисто вышел (vsh поднялся дальше) — is_ram-guard уберёг
снос простора.

## Что дальше (к Linux-драйверам)

- **Доставка прерываний в userspace** — оставшийся механизм фундамента: interrupt-драйверы (и
  Linux `request_irq`) без него не работают (наш демо-драйвер опрашивает). Следующий кусок.
- **Linux-API шим (`lx_emul`)** — поверх этого фундамента: реализовать kmalloc/ioremap/DMA/
  request_irq/driver-model для конкретной подсистемы (Genode dde_linux-стиль). Большой заход.
- Без IOMMU dma-cap = доверие «DMA куда угодно»; настоящая изоляция драйверов — потом (IOMMU).

## Файлы

- `kernel/src/cap.rs` — `Target::Mmio`/`Dma`, `cap::mmio`/`cap::dma`.
- `kernel/src/proc.rs` — `SYS_MMIO_MAP` (31), `SYS_DMA_ALLOC` (32).
- `kernel/src/frame.rs` — `is_ram`; `arch/{riscv64,x86_64}/paging.rs` — guard в `free_private`.
- `kernel/src/init.rs` — токены `mmio:`/`dma`, демо-спавн e1000d; `main.rs` — сеть virtio-net первой.
- `programs/user/src/lib.rs` — `mmio_map`/`dma_alloc`; `bin/e1000d.rs` — e1000 в userspace.

## Связано
- [[e1000]] (тот же драйвер, но в ядре) · [[frame-freeing]] (is_ram при сносе) · [[known-gaps]] · [[todo]]
