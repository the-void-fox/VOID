---
title: Веха 35 — потоки. std::thread, futex, нативный TLS
created: 2026-07-01
tags: [project/void, topic/threads, topic/kernel, topic/std, topic/concurrency]
status: done
---

# Веха 35 — потоки (std::thread)

Ядро научилось планировать **нити внутри процесса**, а порт std (`vendor/rust`)
дорос до настоящих `std::thread`, `Mutex`/`Condvar` на futex и корректного
`thread_local!` через нативный TLS. Это разблокирует sort/rayon/ripgrep
([[void-pkg]]). Проверено на обеих архитектурах обычным Rust-кодом с одного диска.

## Нить = контекст в общей группе

`Proc` получил поле `group` — индекс главной нити (лидера). Нити одной группы
делят `space` (адресное пространство → все страницы, включая кучу), `domain`
(c-space → те же capability) и **кучу лидера** (`heap_brk`, args/env/start_caps
читаются у него); своё у нити — кадр, стек, состояние, TLS. Это модель
POSIX-нитей: нить — не новая единица защиты, полномочий не добавляет, поэтому
`SYS_THREAD_SPAWN` прав и не требует. Пять новых сисколлов (набор дорос до 27):

- **`SYS_THREAD_SPAWN(entry, arg, stack_top)`** — завести нить (тот же `space`/
  `domain`, свой стек; стек userspace выделяет из кучи, страницы приходят ленивым
  фолтом — он резолвится у лидера группы). Возврат — id нити.
- **`SYS_THREAD_EXIT(retval)`** / **`SYS_THREAD_JOIN(tid)`** — завершить нить и
  забрать её `retval` (как `ExecWait`, но для нитей: `JoinWait(tid)`).
- **`SYS_EXIT`** теперь кладёт **всю группу** (возврат из main / `process::exit`
  не переживают прочие нити — семантика POSIX); родитель ждал в `SYS_EXEC` лидера.
- **`SYS_FUTEX(op, uaddr, val, timeout)`** — WAIT/WAKE, ключ = `(space, uaddr)`
  (futex-слова процесса общие для его нитей). Примитив под Mutex/Condvar/Parker.
- **`SYS_SET_TLS(ptr)`** — задать указатель нити (tp на riscv, база `%fs` на x86).

Futex-таймаут честен, пока хоть одна нить бежит (дедлайны проверяются в `resume`
на каждом trap'е из U). Прерываний у нитей нет — планировщик и так вытесняет по
таймеру; contended-путь Mutex спит в `FutexWait`, владелец бежит.

## Нативный TLS — local-exec, две вариации

`has_thread_local = true` + `tls_model = local-exec` (программы статические
no-PIE → TPREL-смещения резолвит линкер, динамических TLS-релокаций нет,
ELF-загрузчику ядра TLS-секции безразличны — приезжают в обычном `PT_LOAD`).
Линкер-скрипт программы добавил `PT_TLS` + `.tdata`/`.tbss` и символы шаблона
(`__tls_start`/`__tdata_end`/`__tls_end`/`__tls_align`). Рантайм pal
(`sys/pal/void/tls.rs`) на старте КАЖДОЙ нити копирует шаблон в свежий блок и
ставит указатель по psABI:

- **riscv64 (Variant I):** `tp` (x4) — на начало блока; переменная по `tp + off`.
- **x86-64 (Variant II):** база `%fs` — на TCB СРАЗУ ЗА блоком; переменная по
  `%fs + (off − block_size)` (отрицательные смещения); `%fs:0` держит адрес TCB.

`block_size = round_up(memsz, align)` считается РОВНО по `__tls_align` (= p_align
PT_TLS): именно им линкер округлял при расчёте TPREL. std сам зовёт `tls::install`
первым делом в `_start` и в трамплине нити — до любого `#[thread_local]`.

## std pal: Thread, futex, синхронизация

`sys/thread/void.rs` — `Thread::new` выделяет стек из кучи, боксит `ThreadInit`,
запускает трамплин (тот СНАЧАЛА строит TLS, потом `init()` ставит `thread::current`
и исполняет замыкание, затем `SYS_THREAD_EXIT`); `join` — `SYS_THREAD_JOIN` +
освобождение стека. `sys/pal/void/futex.rs` даёт `futex_wait/wake`, и void вписан
в futex-ветки `Mutex`/`Condvar`/`RwLock`/`Parker`/`Once` (те же, что у Linux/hermit).
Аллокатор (`sys/alloc/void.rs`) был однопоточным (`SyncUnsafeCell` без лока) —
под нитями это гонка free-list; добавлен спин-лок (не std-Mutex: тот сам на futex
→ рекурсия; на контеншене — `SYS_YIELD`).

## Демо

- `bin/threads` (no_std, void_user, 124 строки) — 4 нити × 50000 инкрементов
  ОДНОГО неатомарного счётчика под самодельным futex-мьютексом (Drepper): итог
  ровно **200000**, join'ы вернули retval. Проверяет ЯДРО без участия std.
- `bin/threads-std` (обычный Rust, 84 строки, ELF 114/76 КиБ) — `std::thread` +
  `Arc<Mutex<u64>>` + `thread_local!`: 4 нити × 20000 = **80000**, плюс
  **тест изоляции TLS** (каждая нить видит своё значение под вытеснением, главная —
  своё). На обеих арх: «✓ потоки, Mutex и thread_local работают», паник 0, чистый
  выход, память ~24.6 МиБ.

## Грабли

1. **x86: `fsbase` затирался на каждом трапе** — лучший баг вехи. `%fs`-база это
   MSR, а не GP-регистр: стаб `trap_entry.s` её не спасает, и `frame = *frame` в
   `handle_user_trap` записывал в слот `fsbase` мусор со стека → следующий `wrmsr`
   грузил мусор → `%fs:0` (self-ptr TCB) читался в никуда → фолт на первом же
   `thread_local` ПОСЛЕ первого трапа (SYS_WRITE от `println`). riscv иммунен: `tp`
   (x4) — обычный GPR, спасается стабом. Фикс — `carry_tls_from`: перенести
   TLS-указатель из прошлого кадра сразу после копии (riscv — no-op).
2. **Аллокатор std был однопоточным** — прямое следствие «потоков нет» прошлых
   вех; под нитями два одновременных alloc портили бы free-list. Спин-лок.
3. **Strict provenance в std** — int↔ptr касты (символы линкера, `Box`-arg через
   регистр) требуют `with_exposed_provenance`/`.addr()`/`.expose_provenance()`,
   как в `sys/alloc/void.rs`.
4. **Первая цель `has_thread_local` + no-op guard** — модуль `destructors`
   компилируется (native TLS), но `run` не зовётся (TLS-деструкторы утекают,
   `guard::enable` no-op) → deny-warnings на «unused `run`». Заглушено
   `#[cfg_attr(target_os = "void", allow(...))]` точечно.

## Отложено (честно)

TLS-деструкторы (утекают на выходе нити — как и TLS-блок/стек нити при желании);
`available_parallelism` = 1 (SMP нет — нити вытесняющие, но CPU один);
`park_timeout`/`wait_timeout`, где ВСЕ нити разом спят по таймеру, завершат сессию
как любой полностью заблокированный процесс (не встречается у rayon/sort).

## Связано
- [[std-port]] (порт std, куда встроились нити) · [[process-contract]] ·
  [[processes]] · [[scheduling]] · [[uutils]] (sort ждал именно этого) ·
  [[void-pkg]] · [[0002-persistent-content-addressed-capability-core]]
