---
title: Веха 38 — linux-abi. неизменённые бинари nixpkgs под персоналией Linux
created: 2026-07-01
tags: [project/void, topic/kernel, topic/packages, topic/linux, topic/abi]
status: done
---

# Веха 38 — linux-abi (бэкенд B)

Бэкенд **B** пакетной дорожки ([[0004-void-pkg]], [[void-pkg]]): неизменённые
статические бинари из **бинарного кэша nixpkgs** работают на VOID под персоналией
Linux. Путь FreeBSD linuxulator / gVisor / Managarm — тонкий транслятор
Linux-syscall'ов **в ядре**, а не порт userland'а. Демо: static-PIE musl `hello`
(и applet'ы busybox) — тот же бинарь, что запустился бы на Linux, только `ecall`/
`syscall` уходит не в ABI VOID, а в таблицу трансляции.

## Почему это ложится (и почему только nix)

Разложение пакета ([[void-pkg]]) оставляет один барьер — **runtime-ABI бинаря**.
Замыкания nix самодостаточны и **не зависят от FHS**: их /nix/store-пути ложатся
на наш store 1:1. Для статического однопоточного CLI Linux-ABI — это ~десятки
syscall'ов (write/writev/brk/mmap/exit…), без страшных clone/futex. deb/rpm/AUR
предполагают глобальный изменяемый /usr — мимо не-FHS ОС.

## Ключевое решение: static-PIE, а не ET_EXEC

Обычный статический Linux-бинарь — `ET_EXEC`, слинкованный по **фиксированному**
0x400000. На обеих наших архитектурах это гибельно:
- **riscv**: 0x400000 — регион VPN[2]=0, чьи подтаблицы **разделяются** между
  процессами и ядром (там MMIO/PLIC). Пользовательский маппинг туда потёк бы во
  все пространства.
- **x86**: 0x400000 = 4 МиБ — **внутри образа ядра** (грузится с 1 МиБ). Отдать
  этот адрес userspace = отдать ему кишки ядра.

Ответ — **static-PIE** (`gcc -static-pie`, `ET_DYN`): грузим в наш user-регион
(0x4000_0000), как родные программы, вся изоляция цела. Бонус: musl `rcrt1`
**сам себя релоцирует** из `_DYNAMIC` — ядро релокации не применяет (сделать это
ещё раз означало бы удвоить их). Ядро лишь кладёт сегменты по базе и строит auxv.
Собираем static-PIE рецептом nixpkgs (оверрайд `-static-pie`) — это по-прежнему
«бинарь по рецепту nix», просто не буквальный ET_EXEC из кэша.

## Ни одной новой инструкции ядра

- **riscv**: linux и VOID зовут ядро одним `ecall` — разводим по флагу личности
  `Proc.linux`. Номера — generic-ABI (`write`=64, `exit_group`=94…).
- **x86-64**: musl зовёт ядро инструкцией `syscall`, которую мы **не включали**
  (EFER.SCE=0) → она даёт **#UD**. Ловим вектор 6 от linux-процесса, читаем опкод
  (0F 05), диспатчим, `rip += 2`. Никаких MSR (LSTAR/STAR/SFMASK), никакого стаба
  входа — переиспользуем существующий trap-кадр. Номера — легаси x86-64
  (`write`=1, `exit_group`=231…). Разные таблицы номеров разводит `linux::decode`.

## Стартовый стек Linux (auxv)

То, что на Linux строит ядро при execve, здесь строит `linux::build_init_stack`:
`argc | argv | NULL | envp | NULL | auxv | AT_NULL`, выше — строки и 16 байт
`AT_RANDOM`. Без корректного **auxv** musl не находит себя: `AT_PHDR/PHENT/PHNUM`
(TLS/self), `AT_ENTRY`, `AT_PAGESZ`, `AT_RANDOM` (канарейка/ГПСЧ), `AT_EXECFN`.
`sp` выровнен на 16 — требование ABI на входе `_start`.

## Память бесплатно поверх ленивой кучи

`brk`/`mmap(MAP_ANONYMOUS)` не требуют нового механизма: отдают хвост **ленивой
кучи** процесса (`heap_brk`, Веха 22), страницы приходят по page fault. `munmap`/
`mprotect`/`madvise` — no-op (bump не освобождает; RELRO→RO чтим как no-op). `mmap`
файла — пока ENOSYS.

## Что работает / отложено

- ✅ `hello` (UTF-8 argv, exit-код) на **обеих** архитектурах — неизменённый
  static-PIE musl из nixpkgs, доставлен мостом (Веха 29), `run bin/lhello`.
- ✅ **busybox** (1.37, ~1.2 МБ, один бинарь): `echo`, `uname -a` (наша Linux/void-
  личность), `true`, `seq`, `printf`, `basename`… — вычислительные applet'ы,
  пишущие в stdout, на **обеих** архитектурах. Мультиплекс по argv (`run bin/busybox
  echo …`). Проверенный набор syscall'ов: set_tid_address, get/set uid/gid,
  rt_sigprocmask, newfstatat(→ENOENT для конфига), uname, ioctl(→ENOTTY), writev.
- ⏳ Файловые applet'ы (`cat FILE`, `ls`, `sh`): нужен мост `openat/getdents/read`
  к персоналии posixfs (личность linux несёт её эндпоинт в старт-cap — задел есть).
  Пока `openat` → ENOENT.
- ⏳ Многопоточное (`clone`/`futex`), сигналы, poll/ppoll — за однопоточным CLI.

## Транслятор — чистые части и диспетчер

- `kernel/src/linux.rs` — errno, `Lx` + `decode(nr)` (per-arch), `build_init_stack`,
  `fill_utsname`/`fill_stat_chr`, `tick_ns`.
- `kernel/src/proc.rs` — `Proc.linux`, `spawn_linux_locked`, `linux_syscall`
  (диспетчер: нужны таблица процессов, ленивая куча, консоль), `#UD`→syscall на x86.
- `kernel/src/elf.rs` — `load_pie` (ET_DYN, без релокаций), `is_pie` (авто-детект
  пути в `SYS_EXEC`: ET_EXEC → родной, ET_DYN → linux).
- Контракт arch пополнен `user_pc`/`skip_syscall_insn` (riscv sepc+4 / x86 rip+2).

## Доставка

Своя C-программа — одной командой (musl-кросс из кэша), затем мост:
```sh
nix-shell -p pkgsCross.musl64.buildPackages.gcc --run \
  'x86_64-unknown-linux-musl-gcc -static-pie -fPIE prog.c -o prog'   # riscv64-musl для RISC-V
tools/.../void-store-import void-disk.img put prog bin/x86_64/lprog
# в vsh:  run bin/lprog
```

**busybox** (static-PIE из исходников — грабли, стоившие времени):
- cc-wrapper nixpkgs по умолчанию добавляет `-pie`; на промежуточных `ld -r`
  busybox'а это `-r and -pie may not be used together` → `export NIX_HARDENING_ENABLE=""`.
- busybox тащит `CONFIG_EXTRA_LDFLAGS` (`-static-pie`) и в частичные `ld -r` →
  патч `scripts/Makefile.lib`: `ld_flags = $(filter-out -static-pie -static -pie …)`
  (флаг нужен только финальному gcc-линку).
- riscv-`defconfig` включает x86-only SHA-NI → снять `CONFIG_SHA1/256_HWACCEL`; `CONFIG_TC` тоже.
```sh
make defconfig ARCH=<x86_64|riscv> CROSS_COMPILE=<triple>-
# CONFIG_STATIC off; EXTRA_CFLAGS="-fPIE"; EXTRA_LDFLAGS="-static-pie"; патч ld_flags
NIX_HARDENING_ENABLE="" make -j4 …    # → ET_DYN busybox
```

## Связано
- [[0004-void-pkg]] · [[void-pkg]] (лестница, ступень 4) · [[nixpkgs-cross]] (бэкенд A)
- [[process-heap]] (ленивая куча под brk/mmap) · [[process-contract]] · [[elf-userspace]]
