---
title: Веха 25 — x86_64 оживает. PVH boot, IDT, LAPIC, PML4, контексты
created: 2026-07-01
tags: [project/void, topic/kernel, topic/x86, topic/boot, lang/rust]
status: active
---

# Веха 25 — x86_64 оживает

Заглушки Вехи 24 ([[arch-boundary]]) наполнены: тот же образ общего кода, что на RISC-V,
загружается на `qemu-system-x86_64` и проходит всю ядерную половину демо — store (в RAM,
диска пока нет), capability, планировщик с вытеснением LAPIC-таймером, async-executor,
GC, commit. Userspace (ring3, syscall-путь, программы) — Веха 26+, поэтому
`USERSPACE_READY = false` и kmain честно печатает `[skip]` на процессных демо.

## Загрузка: PVH вместо Limine (отклонение от роадмапа)

Роадмап называл Limine, сделан **PVH direct boot**: в ELF кладётся нота
`XEN_ELFNOTE_PHYS32_ENTRY` (тип 18), и QEMU грузит ядро прямо через `-kernel`, стартуя
наш 32-битный вход. Почему:

- **Идентичность VA==PA сохраняется** — та же модель памяти, что на RISC-V (OpenSBI →
  физический ELF). Limine затаскивает higher-half + HHDM, а это переписывание `frame`,
  store и DMA-буферов ради загрузчика — телега впереди лошади.
- **Ни внешних бинарей, ни ISO**: рабочий цикл остаётся `cargo run` (раннер в
  `.cargo/config.toml`), паритет с riscv-дорожкой.
- Цена — свой трамплин 32→64 (~60 строк asm): GDT, PAE, временные идентичные таблицы
  (2 МиБ страницы на 4 ГиБ), `EFER.LME|NXE`, `CR0.PG|WP`, far-return в 64-битный сегмент.
  Настоящие таблицы строит уже Rust (`paging::init`) и перещёлкивает CR3 — «как до/после
  `paging::init` на RISC-V».

## Что наполнено (`arch/x86_64/`)

- **entry.s** — PVH-нота + трамплин 32→64; `EBX` (адрес start_info) едет в `kmain` вторым
  аргументом, как dtb на riscv.
- **trap.rs + trap_entry.s** — IDT на 256 шлюзов, стабы векторов 0–32 + spurious 0xFF
  (вектора с аппаратным error code получают его, остальным стаб подкладывает 0 — кадр
  всегда одной формы); `TrapFrame` с методами контракта (`syscall_num`=rax,
  аргументы rdi/rsi/rdx/r10/r8/r9/rbx — SysV + rbx как седьмой); фатальный дамп с cr2.
- **lapic.rs** — xAPIC по MMIO `0xfee0_0000`: spurious-вектор, one-shot LVT-таймер
  (вектор 32), EOI; legacy-PIC замаскирован. Квант ~20 мс на шине APIC QEMU.
- **paging.rs** — 4-уровневые таблицы: direct map RAM, W^X ядра битом NX (`EFER.NXE`),
  окно virtio-mmio отображено — общий драйвер честно проберётся и не найдёт устройства.
- **switch.s + Context** — callee-saved x86_64 (rsp/rbx/rbp/r12–r15), адрес возобновления
  на стеке задачи, трамплин `call rbx → task_exit` — калька riscv-схемы.
- Семантика `TrapFrame::set_start_arg` появилась в контракте ОБЕИХ арх: на riscv `a0..`
  несут и аргументы и возвраты, на x86 это разные регистры (rax ≠ rdi) — общий `proc`
  теперь говорит «стартовый аргумент», а не «регистр результата».

## Грабли

- **PVH ABI не определяет esp.** Трамплин делал `push`/`retf` до установки стека —
  QEMU молча висел (ни ноты об ошибке, ни вывода: push уходил в никуда и retf прыгал
  по мусору). Одна строка `mov esp, offset _boot_stack_top` — и всё ожило. Мораль:
  в чужом ABI неопределённое = несуществующее.
- **`rdmsr` клоббирует edx** — отладочные маячки `out dx, al` после чтения EFER писали
  в порт 0 и «пропадали». На боевой код не влияло, но полчаса недоумения при бисекции.
- **`push offset _start64` в .code32** LLVM собирает 16-битной релокацией
  (`R_X86_64_16 out of range`) — адрес кладётся через регистр.
- Нота должна попасть в **PT_NOTE**: секция `.note.Xen` типа `@note` + отдельная
  выходная секция в `linker-x86_64.ld` ДО `/DISCARD/ *(.note .note.*)`.

## Проверено

1. Оба таргета собираются без предупреждений; riscv-образ бинарно тот же путь.
2. x86_64, `cargo run --target x86_64-unknown-none`: PVH → баннер → «x86_64 4-level
   включён (direct map + W^X)» → virtio-blk честно не найден → store «ПЕРВЫЙ запуск»
   (RAM) → 13 посеяно → cap-демо целиком → sched X/Y (вытеснения LAPIC: 2) → async →
   `[skip]` процессных демо → GC 18/15 → commit → idle.
3. riscv-регрессия: свежий диск — сев 13, полная vsh-сессия (`ls · cat · echo … >
   m25.txt · run bin/hello · exit`); перезагрузка — 13 актуально (hello тот же
   `32579bfeef…`), `m25.txt` жив, PANIC/FATAL 0.

## Дальше (Веха 26+ — userspace x86_64)

TSS + сегменты ring3 + `enter_user` (iretq) и syscall-путь (`syscall/sysret` или int),
маски прерываний сессий, IOAPIC (консольный RX IRQ4, virtio), virtio-pci (диск →
персистентность), сборка `programs/user` под x86 (asm-шимы syscall'ов) и арх-измерение
корней `bin/<arch>/<имя>`.

## Связано
- [[arch-boundary]] (Веха 24 — пререквизит) · [[0005-multiarch-arch-boundary|ADR 0005]]
- [[elf-userspace]] · [[sv39-paging]] (riscv-аналог памяти) · [[scheduling]]
- [[0004-void-pkg]] — Фаза 4 · [[todo]]
