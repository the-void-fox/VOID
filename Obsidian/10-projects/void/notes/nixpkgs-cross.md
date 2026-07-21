---
title: Веха 36 — nixpkgs-cross. C-мир поверх void-libc
created: 2026-07-01
tags: [project/void, topic/packages, topic/nix, topic/libc, topic/c]
status: done
---

# Веха 36 — nixpkgs-cross (C-мир поверх void-libc)

Ступень 3 лестницы [[void-pkg]] (бэкенд A, магистраль ADR [[0004-void-pkg]]):
**настоящие C-программы по рецептам nixpkgs работают на VOID**. GNU hello
(autotools + gnulib) и bzip2 (Makefile, честная утилита) собираются
`nix-build nix -A <arch>.<pkg>` на хосте, доставляются мостом и работают в vsh
на обеих архитектурах: `hello --greeting=Привет-от-C-мира` печатает
UTF-8-аргумент, bzip2 сжимает и разжимает файлы персоналии байт-в-байт.

## Декомпозиция: nixpkgs — цех, newlib — libc, глю — наше

Полный «свой триплет `*-void` в nixpkgs» не понадобился. nixpkgs уже держит
кросс-тулчейны bare-metal с newlib в бинарном кэше: `pkgsCross.riscv64-embedded`
(gcc 15.2 + newlib 4.5, `riscv64-none-elf`, дефолт РОВНО rv64gc/lp64d) и
`pkgsCross.x86_64-embedded` (`x86_64-elf`). newlib даёт весь «верх» libc
(stdio/malloc/string/printf) и десятилетиями портируется на новые ОС дюжиной
стабов — ровно этот контракт реализует **void-libc**
(`Code/programs/void-libc`, ~610 строк C/asm на ОБЕ архитектуры):

- `crt0.S` — вход по `e_entry`: riscv ставит `gp` (`__global_pointer$` дал
  общий linker.ld — gcc адресует `.sdata` относительно gp), выровнять стек,
  в `_start_c`;
- `void.c` — старт (куча одним ленивым `SYS_MAP` 16 МиБ + argv/env из
  `SYS_ARGS` + `.init_array` + `main` + `exit`) и стабы newlib
  (`_write/_read/_open/_close/_lseek/_fstat/_stat/_unlink/_rename/_sbrk/...`):
  консоль — fd 0/1/2 → `SYS_READ`/`SYS_WRITE`, файлы — IPC к posixfs
  (старт-cap слот 0, те же op-коды, что у std-порта), `_sbrk` двигает границу
  внутри резерва (физика приходит фолтами);
- `void.specs` — специи gcc: `*startfile` → наш `void-crt0.o` + `-T void.ld`,
  `*lib` → группа `-( -lvoid -lc -lgcc -)`. После них ЛЮБОЙ вызов кросс-gcc
  (`gcc -B… -specs=void.specs prog.c`) линкует готовый VOID-ELF — включая
  configure-пробы autotools.

Раскладку задаёт ОБЩИЙ с Rust-программами `programs/user/linker.ld` (один
источник правды: база 0x4000_0000, W^X-сегменты); он дорос до массивов
конструкторов C (`.init_array`/`.fini_array`, KEEP, `:rodata`) и
`__global_pointer$` — Rust-объекты их не эмитят, для них секции пусты.

`nix/default.nix` (119 строк) — обвязка: `void-libc`-деривация, `voidify`
(рецепт nixpkgs → пакет VOID: `env.NIX_CFLAGS_COMPILE += -B…/-specs=…`,
cc-wrapper добавляет их к каждому вызову) и сами `hello`/`bzip2`.

## Ядро: FP/SSE-контекст x86 (гвоздь вехи)

Первый же C-бинарь на x86 упал `#UD`: gcc-код baseline x86-64 использует SSE,
а ядро не включало `CR4.OSFXSR` — riscv получил FP-контекст ещё в Вехе 32
(uutils), x86 жил без него, потому что std-порт собран под soft-float.
Решение зеркалит riscv, но через пару методов контракта:

- `entry.s`: `CR4 |= OSFXSR|OSXMMEXCPT` (рядом с PAE);
- `TrapFrame` (x86) вырос на `FxArea` — 512-байтный образ `fxsave64` с честным
  Default (FCW=0x037F, MXCSR=0x1F80); как и `fsbase`, стаб трапа его не пишет;
- `save_fp()` в `handle_user_trap` сразу после копии кадра: ядро собрано с
  soft-float и XMM НЕ трогает, поэтому живые регистры на входе в трап —
  состояние затрапившего процесса; `restore_fp()` — в `enter_user`, рядом с
  `wrmsr fsbase`. `fxsave64` требует выравнивания 16 — кадры в таблице
  процессов не выровнены, работает через выровненный скретч (однопроцессорное
  ядро, в обработчиках IF=0 — реентерабельности нет);
- riscv: обе стороны no-op (f0..f31 в кадре спасает сам стаб с Вехи 32), но
  точки вызова те же — парность контракта.

## Грабли (все — «неизвестная ОС» для чужого кода)

1. **Имена стабов**: newlib riscv собран под `_write`, newlib x86_64 — под
   голое `write`. Одно тело + алиасы `__typeof(_write) write` — заодно это
   честные POSIX-обёртки для программ, зовущих `open/read/write` напрямую.
2. **`crt0.o%s` из specs нашёл ЧУЖОЙ crt0**: обёртка nixpkgs ставит `-B` newlib
   раньше нашего, и gcc взял их crt0 (libgloss, ждёт `__bss_start`). Лечится
   уникальным именем `void-crt0.o`.
3. **`__structuredAttrs`**: у современных рецептов nixpkgs атрибут деривации НЕ
   попадает в окружение сборки — `NIX_CFLAGS_COMPILE` надо класть через
   `env.*`, иначе cc-wrapper его не видит (сборка «работает», но без наших
   флагов).
4. **gnulib на неизвестной ОС**: `getprogname` — `#error not ported`,
   `getdtablesize` — замена, безусловно зовущая `getrlimit`, которого нет в
   заголовках newlib; замена `fcntl` тянет невключённый модуль `dupfd`.
   Рецептура: дать функции в глю + сказать configure, что они есть
   (`ac_cv_func_*=yes`), а декларации дописать в `config.h` постконфигуром
   (функцию gnulib ищет ЛИНКОВКОЙ, а декларацию — в stdlib.h newlib, где её
   нет). bzip2 аналогично: `lstat`/`utime`/`fchmod`/`fchown` даёт глю
   (симлинков и прав у персоналии нет — lstat ≡ stat, chmod-семейство — 0).
5. **POSIX-семантика open**: посикс-персоналия создаёт файл при ЛЮБОМ open —
   `fopen("нет","r")` обязан вернуть NULL, поэтому `_open` без `O_CREAT`
   сначала спрашивает `OP_STAT`. Иначе bzip2 «находил» несуществующие файлы.

## Демо и цифры

- `run bin/hello-gnu` → «Hello, world!»; `--version` — полный баннер GNU;
  `--greeting=Привет-от-C-мира` — UTF-8 через argv → printf (x86_64).
- `run bin/bzip2 -z c.txt` → `c.txt.bz2` (исходник честно unlink'ается),
  `-d` возвращает, `cat` — байт-в-байт. На обеих архитектурах.
- ELF: hello 116/147 КиБ, bzip2 158/185 КиБ (riscv/x86, stripped) — C-мир
  тяжелее no_std (6–13 КиБ), сравним со std-миром (85–122 КиБ).
- Регрессия: боты обеих арх, threads (200000), threads-std (80000 + TLS),
  coreutils — чисто, 0 паник. FP-контекст ничего не сломал (главный риск —
  x86 fxrstor рядом с fsbase — проверен threads-std).

## Отложено (честно)

Файлы ≤4 КиБ и плоские имена ≤32 (лимиты персоналии, не глю); `times`/`clock`
не реализованы (никто не спросил); TLS у C-программ нет (`errno` работает через
reent newlib однопоточно — нитей C-миру не обещано); флаги
`ac_cv_*` подобраны под hello — следующий autotools-пакет может попросить свои
(это нормальная цена gnulib).

## Связано
- [[void-pkg]] (лестница: ступень 3 взята) · [[0004-void-pkg]] ·
  [[std-port]] (тот же ABI, другой рантайм) · [[uutils]] ·
  [[posix-personality]] (весь файловый мир C идёт через неё) ·
  [[store-bridge]] (доставка) · [[threads]] (FP-контекст x86 — их недостающая половина)
