---
title: Веха 24 — граница архитектур. arch/{riscv64,x86_64} за узким контрактом
created: 2026-07-01
tags: [project/void, topic/kernel, topic/multiarch, topic/x86, lang/rust]
status: active
---

# Веха 24 — граница архитектур

Решение — [[0005-multiarch-arch-boundary|ADR 0005]]; здесь — что и как переезжало.
Итог вехи: `cargo build` (riscv64) и `cargo build --target x86_64-unknown-none` собираются
из одного дерева, **оба без единого предупреждения**; riscv-образ прошёл полную регрессию
(два прогона, персистентность, vsh-сессия), x86-образ — заглушка, линкующаяся в честный ELF.

## Что уехало в `arch/riscv64/`

Файлами, почти без правок: `csr.rs`, `sbi.rs`, `plic.rs`, `uart.rs`, `paging.rs`, `trap.rs`,
`context.rs` + весь ассемблер (`entry.s`, `trap_entry.s`, `switch.s`, `enter_user.s`).
Наружу они больше не видны — только `arch/riscv64/mod.rs`, реализующий контракт
(~30 функций, 2 типа, 6 констант; сам контракт задокументирован в `arch/mod.rs`).

## Как изменился общий код

- **`proc`**: ни одного номера регистра — `frame.arg(2)`, `frame.set_ret(n)`,
  `frame.advance()` вместо `regs[12]`/`sepc += 4`; `satp` стал токеном
  `space = arch::space_token(root)`; `sfence.vma`/`wfi` стали `arch::flush_tlb()`/
  `arch::wait_for_interrupt()`; главный выигрыш — `handle_user_trap(frame, UserTrap)`
  принимает СМЫСЛ (Syscall / PageFault{va,kind} / TimerTick), классификация scause уехала
  в арх.
- **`timer`**: остались счётчик тиков и политика («на тике вычерпай консоль, уступи»);
  железо (SBI + rdtime + квант в тиках таймбазы) — в архе.
- **`sched`**: `Context::new_task(entry, sp)` вместо ручной раскладки `ra/sp/s0` (трамплин —
  внутренняя деталь арха).
- **`elf`**: `arch::ELF_MACHINE` вместо EM_RISCV, флаги `arch::MAP_*` вместо PTE-битов Sv39.
- **`main`**: блок «PLIC + UART-приём» сжался в `arch::init_device_interrupts()`; баннер
  печатает `arch::MM_NAME`.

## Заглушка `arch/x86_64/`

Настоящее уже сейчас: COM1-вывод (печать баннера при будущем bring-up), rflags.IF/cli/sti/hlt.
Остальное — `unimplemented!("x86_64: заглушка Вехи 24 …")` с точным именем недостающего
(GDT/IDT, PML4, LAPIC, iretq/sysret, switch-асм). Плюс `linker-x86_64.ld` (база 1 МиБ,
те же символы `_kernel_end` и др. для общего кода) и entry: стек → `kmain(0,0)` → hlt.
Bring-up будет наполнять эти функции, не трогая общий код.

## Грабли

- **dead-code на заглушке**: раз x86-трап не зовёт `proc::handle_user_trap`, компилятор
  считает мёртвым ползадачи ядра (35 предупреждений). Лечение стало фичей — функция
  `trap_dispatch_obligations`: перечисляет кодом все хуки, которые обязан звать будущий
  обработчик (тик таймера, IRQ диска, классификация всех вариантов `UserTrap`). Документация
  и подавление в одном месте.
- **`x86_64-unknown-none` по умолчанию собирает PIE** — с классическим линкер-скриптом нужен
  `-C relocation-model=static` (в `.cargo/config.toml` на таргет).
- **rustup подтянул новый stable** (1.96 → 1.97) при добавлении таргета в
  `rust-toolchain.toml` (канал `stable` не запинен на версию): сборка и регрессия чисты,
  content-id программ не поменялись (сборка воспроизводима), но помнить об этом стоит.
- Ложная тревога на проверке: «пропавший ввод vsh» оказался пропавшим НЕ вводом, а
  feed-скриптом (очистился scratchpad предыдущей сессии) — час погони за несуществующей
  регрессией RX. Диагноз здорового ядра: прямые `printf | cargo run` тесты доставили ввод
  и через дренаж на тиках, и в спящий `wait_stdin`.

## Проверено

1. Оба таргета: `cargo build` и `cargo build --target x86_64-unknown-none` — 0 предупреждений.
2. riscv, свежий диск: 13 посеяно, демо Вех 8–22 целы (46 вытеснений, heap/crash, IRQ диска),
   vsh: `ls · cat · echo … > m24.txt · run bin/hello · exit`.
3. riscv, перезагрузка: 13 актуально (hello тот же `32579bfeef…`), greeting/`.cspace`-cap
   восстановлены, `m24.txt` жив, PANIC 0.

## Связано
- [[0005-multiarch-arch-boundary]] — решение и альтернативы
- [[elf-userspace]] (Веха 23 — пререквизит) · [[user-mode]] · [[sv39-paging]] · [[processes]]
- [[0004-void-pkg]] — Фаза 4 · [[todo]]
