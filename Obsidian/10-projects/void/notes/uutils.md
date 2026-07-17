---
title: Веха 32 — uutils. настоящие coreutils в vsh на обеих архитектурах
created: 2026-07-01
tags: [project/void, topic/packages, topic/rust, topic/userspace]
status: done
---

# Веха 32 — uutils: первая честная точка самодостаточности

**Критерий вехи** (из [[void-pkg]]): кросс-сборка uutils/coreutils, импорт мостом,
настоящие `ls/cat/cp` в vsh с одного диска на обеих архитектурах. **Выполнен**:
multicall-бинарь coreutils 0.9.0 с восемью утилитами — `ls · cat · cp · wc ·
mv · rm · head · echo` — собирается обычным `cargo build` тулчейном `void` и
работает под обеими архитектурами. Межарх-демо: `cp` на x86_64 создаёт файл,
`wc` на riscv64 его считает (` 1 10 60 from-x86.txt`). Бонус честности: `wc
нет-такого-файла` отвечает по-русски — это наше сообщение из `sys/fs/void.rs`
проехало сквозь io::Error → uucore → вывод утилиты.

Веха съела оба «осознанных ограничения» Вехи 31 ([[std-port]]): аллокатор
больше не течёт, std::fs существует.

## std-порт дорос до файлов (vendor/rust, ветка void, `6138e8cb`)

- **`sys/fs/void.rs` — std::fs по IPC к posixfs** ([[posix-personality]]).
  Эндпоинт — стартовая capability, слот 0 ([[process-contract]] — «preopen»
  WASI): у кого нет права, у того нет и файловой системы. Протокол — зеркало
  `void_user::posix` (op = opcode|fd<<8|mode<<16, кадры ≤512 Б). `File` =
  `FileDesc` (слот сервера + клиентское зеркало курсора); `Drop` = close =
  атомарный коммит файла в store. Тонкости: сервер создаёт файл при любом
  open → контракты `create_new`/`!create` обеспечиваются pre-stat'ами; чтение
  в буфер <512 Б идёт через временный и откатывает лишнее `OP_SEEK`'ом (сервер
  двигает курсор на размер СВОЕГО ответа); запись зеркалит серверный кламп
  4 КиБ и честно отвечает `StorageFull`; `set_permissions` = Ok (прав нет —
  «установить» их тривиально; иначе cp падает на финальном шаге).
- **`sys/alloc/void.rs` — free-list вместо bump**. Ключевая идея: `dealloc`
  получает `Layout`, поэтому занятые блоки не несут заголовков — метаданные
  (дырки `[size|next]`) живут только в свободной памяти. Адресно-упорядоченный
  first-fit с коалесингом обоих соседей; всё кратно 16 (MIN_ALIGN обеих арх);
  арена по-прежнему ленивый SYS_MAP на 16 МиБ. hello-std гоняет 100 ×
  (1 МиБ alloc+free) в этой арене — bump умер бы на 17-й итерации.
- **`std::os::fd` для void** — его require'ит uucore (`OwnedFd`/`AsFd` в
  io-обвязке). Третий не-unix путь в os/fd после hermit/motor: RawFd = слот
  posixfs, `OwnedFd::drop` → close по IPC, net/try_clone исключены как у
  trusty, у never-типных пайпов включён готовый трейт-модуль. Оговорка:
  константы STDIN/STDOUT_FILENO = 0/1/2 формальны (stdio в VOID — SYS_WRITE/
  SYS_READ, а не слоты персоналии; коллизию с реальными слотами 0..15 никто
  не разыменовывает).
- **SystemTime с фиктивной базой** (2026-07-01 + время с загрузки): RTC нет,
  но `ls` живёт арифметикой `now − 6 месяцев` — с базой около нуля это паника
  на underflow. Дата неверна и не претендует; порядок/разности в сессии
  корректны; mtime файлов персоналия всё равно не хранит.
- `abi.rs`: полный 7-аргументный syscall (SYS_CALL; на x86 7-й аргумент через
  `xchg rbx` — LLVM резервирует rbx), `sys_call`/`sys_start_cap`;
  `sys/paths/void.rs`: getcwd = «/» (персоналия плоская).

## Ядро под настоящую нагрузку

- **FP-мина Вехи 31 взорвалась и обезврежена**: uutils считают во float
  (форматирование, clap), а `sstatus.FS` был Off → первая FP-инструкция =
  illegal instruction (trap 0x2), «неожиданный trap из U». Теперь TrapFrame
  несёт f0..f31 + fcsr; `new_user` ставит FS=Initial; trap_entry.s сохраняет
  FP только на trap'е из U (там fsd легален — FS процесса ≥ Initial, а ядро
  своих float'ов не имеет), enter_user.s восстанавливает. Таргет ядра
  собран без D — вокруг ручных fsd/fld `.option push / .option arch, +d /
  .option pop`. x86-таргет std — софтфлот без SSE, там мины не было.
- **Стек процесса 64 КиБ → 256 КиБ**: cat/wc кладут на стек буферы по 64 КиБ
  (`[0; 65536]`) — переполнение выглядело как store-фолт на 66 КиБ ниже
  вершины стека.
- **Куча ядра 8 МиБ → 16 МиБ**: 2.2-МиБ ELF при exec живёт в куче дважды
  (кэш store + копия загрузчика), GC-обход грузит все достижимые объекты.

## Форк uutils (сабмодуль vendor/coreutils, ветка void, `135bfd9`)

Подход: **VOID — «третье семейство» с профилем возможностей WASI** (плоская
ФС, нет прав/симлинков/потоков/сокетов), поэтому почти каждая заплатка —
«расширить готовую WASI-ветку на `target_os = "void"`». Дерево заплаток:

- `uucore`: rustix → `[target.'cfg(any(unix, windows))']` (на void не
  собирается — errno не знает такой ОС — и не нужен); fs.rs — FileInformation
  хранит `std::fs::Metadata` (ветки WASI), сравнение по типу+размеру; io.rs —
  `Stdio::from(File::from(fd))` вместо отсутствующего `From<OwnedFd>`;
  fsext.rs — `read_fs_list` = «маунтов нет» (список aix/redox/…/void).
- `uu_cat`, `uu_cp`: rustix → cfg(unix|wasi) — их код уже был за этими cfg;
  symlink-ветки cp и `is_unsafe_overwrite` cat — WASI-заглушки.
- `uu_ls`: `hostname` исключён из зависимостей (крейт не знает void),
  гиперссылки OSC-8 с пустым хостом; `uu_mv`: `rename_symlink_fallback` —
  WASI-заглушка.
- `uu_wc`: bytecount БЕЗ `runtime-dispatch-simd` — фича безусловно компилирует
  SSE2/AVX2-пути, которые на софтфлот-x86 валят LLVM («Do not know how to
  split the result of this operator»); остаётся SWAR-скаляр.
- `patches/{console,filetime}` + `[patch.crates-io]`: у обоих крейтов ЕСТЬ
  wasm/wasi-стабы (терминал «не терминал», времена «не поддержаны») — патчи
  только расширяют их cfg на void. console тянется indicatif'ом (прогресс
  cp/mv/rm), filetime — cp.
- `sort` отложен: тянет rayon (потоки — у нас честный unsupported) и
  rand/getrandom (нужен кастомный бэкенд). Не блокер вехи.

## Сборка (рецепт)

```sh
# из корня репозитория VOID, тулчейн void уже слинкован (см. std-port.md)
nix-shell --run "cd vendor/coreutils && \
  RUSTUP_TOOLCHAIN=void \
  CARGO_TARGET_RISCV64GC_UNKNOWN_VOID_ELF_RUSTFLAGS='-C link-arg=-T$PWD/Code/programs/user/linker.ld' \
  CARGO_PROFILE_RELEASE_PANIC=abort CARGO_PROFILE_RELEASE_OPT_LEVEL=s \
  cargo build --release --target riscv64gc-unknown-void-elf \
    --no-default-features --features cat,echo,wc,head,ls,cp,mv,rm -p coreutils"
# x86: тот же вызов с CARGO_TARGET_X86_64_UNKNOWN_VOID_RUSTFLAGS и --target x86_64-unknown-void
# импорт: void-store-import put <elf> bin/<arch>/coreutils
```

ELF: 2 360 720 Б (riscv) / 1 857 888 Б (x86). Запуск: `run bin/coreutils ls`
(argv[0] — имя корня, multicall берёт утилиту из argv[1]).

## Грабли (по свежим следам)

1. **RUSTFLAGS глобальный ломает proc-macros**: link-arg с нашим линкер-скриптом
   применяется и к хостовым сборкам → syn/quote «can't find crate». Только
   `CARGO_TARGET_<T>_RUSTFLAGS`.
2. **После пересборки std — `rm -rf target` у std-программ**: cargo не
   отслеживает sysroot, полусвежий кэш даёт бессвязные E0463; инкрементальная
   жизнь возможна только пока std не трогали.
3. Цепочка несобирающихся крейтов разматывается ПО ОДНОМУ (cargo стопается на
   первом): errno → console → getrandom(sort) → hostname → filetime →
   bytecount. Каждый следующий виден только после починки предыдущего.
4. **LLVM-легализация на софтфлот-x86** — ошибка вылезает на самой ПОСЛЕДНЕЙ
   стадии (кодоген бина) и без имени виновника; бисекция по фичам утилит
   нашла wc/bytecount за два шага.
5. FP-контекст: `.option arch, +d` обязателен — таргет ядра без D, и это
   правильно (ядро не должно ЭМИТИТЬ float, но обязано СОХРАНЯТЬ чужие).
6. Пакетная подача ввода в QEMU-демо глотает первые байты команды, пришедшие
   в окно завершения ребёнка (заметно на длинных `run bin/coreutils …`);
   живой клавиатуре не грозит, скриптам — посимвольная подача с паузами.

## Цена и честные оговорки

- RAM-пик демо: ~37 МиБ riscv / ~30 МиБ x86 (куча ядра 16 МиБ — резерв под
  двойную жизнь ELF; страницы uutils-процессов). Всё ещё вдвое меньше
  Linux-гостя (~80 МиБ), но «≈12 МиБ» Вехи 31 остались в прошлом — README
  обновлён.
- Файл ≤ 4 КиБ, имя ≤ 32 Б, каталог один (персоналия v1) — uutils работают
  в этих рамках; `ls -l` покажет нули вместо дат (mtime нет).
- `SystemTime` врёт дату намеренно (см. выше); в заметке std-port это
  ограничение снято с «unsupported» на «фиктивная база».

## Дальше

Лестница [[void-pkg]] п.2 закрыта. Кандидаты следующего шага: политика
коммитов store ([[commit-policy]] — перед реальным железом), virtio-net,
nixpkgs-cross (C-мир через VOID-libc), checkpoint процессов, sort/потоки
(std::thread или зелёные нити?).

## Связано
- [[std-port]] · [[process-contract]] · [[store-bridge]] · [[void-pkg]] ·
  [[0004-void-pkg]] · [[posix-personality]]
