---
title: Веха 31 — std-порт. таргеты *-unknown-void, pal, тулчейн из форка
created: 2026-07-01
tags: [project/void, topic/packages, topic/rust, topic/toolchain, milestone]
status: done
---

# Веха 31 — порт Rust std на VOID

Критерий вехи выполнен дословно: `fn main() { println!("…") }` — обычный Rust без
`no_std`/`no_main`/шимов — собирается обычным cargo и работает в VOID. Сверх критерия:
куча, argv/env, HashMap (со случайными сидами), Instant/Duration, коды выхода.
Обе архитектуры, 0 паник. Форк: ветка `void` в `vendor/rust`, коммит `0d7db89a`.

## Витрина (vsh, riscv64 и x86_64 с одного диска)

```
vsh> run bin/hello-std мир std
[hello-std] Привет от НАСТОЯЩЕЙ std на VOID!
[hello-std] argv: ["bin/hello-std", "мир", "std"]
[hello-std] env ARCH=riscv64 SYSTEM=void        ← на x86 честно ARCH=x86_64
[hello-std] sort 100k + сумма = 5000050000 за 32.8089ms
[hello-std] HashMap работает: 2 записи
[hello-std] выходим с кодом 7 — проверка exit-кода
vsh: program exited, code 7
```

ELF: riscv 101 КиБ / x86 72 КиБ (release, opt-level=s, lto). Доставка — мостом
Вехи 29 (`void-store-import put … bin/<arch>/hello-std`), сева в ядре нет.

## Таргеты (rustc_target, tier 3 private)

- `riscv64gc-unknown-void-elf`: RV64GC, lp64d, medium code model — как ядро.
- `x86_64-unknown-void`: **softfloat, без SSE** — ядро не сохраняет FPU-состояние
  процессов, векторные регистры запрещены на уровне таргета (riscv: gc включает
  FP — мина отмечена, до первого float нужен sstatus.FS + сохранение f-регистров).
- Оба: static/no-PIE (наш ELF-загрузчик не делает релокаций), panic=abort,
  rust-lld, `has_thread_local = false`, линкер-скрипт — общий `programs/user/linker.ld`
  (база 0x4000_0000, W^X-сегменты) через build.rs крейта-программы.

## Порт std (`target_os = "void"`)

По образцу двух свежих маленьких портов в 1.97 — `motor` (Rust-ОС на syscall'ах)
и `vexos` (минимализм time/thread). Слои:

| область | реализация |
|---|---|
| pal/void | `_start` (вход из e_entry) → `main` → SYS_EXIT; abi.rs — syscall-шимы (зеркало void_user) |
| stdio | SYS_WRITE / SYS_READ (stdin блокируется честно) |
| args/env | SYS_ARGS(0/1) — NUL-блобы контракта Вехи 30; setenv = unsupported (окружение задаёт родитель) |
| alloc | bump-арена над ленивым SYS_MAP 16 МиБ; **dealloc = no-op (утечка)** — MVP до долгоживущих потребителей |
| time | Instant = rdtime/rdtsc·TICK_NS; SystemTime = unsupported (RTC нет) |
| thread | sleep/yield честные (SYS_YIELD + Instant); spawn = unsupported; thread_local = statik |
| random | xorshift от rdtime — НЕ crypto, хватает сидам HashMap |
| fs/net/process | unsupported (fs — кандидат Вехи 32: IPC к posixfs по start-cap) |

Плюс: `Os::Void` в enum спеков, `build.rs` std (не-restricted платформа),
`env_consts` (OS="void"), `compiler-builtins-mem` в bootstrap (libc нет —
memcpy/memset из compiler_builtins, как у zkvm).

## Сборка тулчейна (вся стратегия)

Окружение — `toolchain-shell.nix` (LLVM 22.1.7 из nixpkgs + zlib/zstd/libxml2).
`vendor/rust/bootstrap.toml` (в .gitignore rust'а, канон — здесь):

```toml
[build]
rustc = "~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc"  # stage0 = наш stable
cargo = "…/bin/cargo"
patch-binaries-for-nix = true
docs = false
extended = false
[llvm]
download-ci-llvm = false
[target.x86_64-unknown-linux-gnu]
llvm-config = "/nix/store/…-llvm-22.1.7-dev/bin/llvm-config"
[rust]
channel = "dev"
download-rustc = false
incremental = false
lld = false          # без сабмодуля llvm-project lld не собрать — см. симлинк
llvm-tools = false
```

Рецепт:
```sh
nix-shell toolchain-shell.nix --run "cd vendor/rust && \
  python3 x.py build --stage 1 library \
  --target x86_64-unknown-linux-gnu,riscv64gc-unknown-void-elf,x86_64-unknown-void"
rustup toolchain link void vendor/rust/build/x86_64-unknown-linux-gnu/stage1
ln -sf ~/.rustup/toolchains/stable-*/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld \
  vendor/rust/build/x86_64-unknown-linux-gnu/stage1/lib/rustlib/x86_64-unknown-linux-gnu/bin/
```
stage1 rustc — ~40 мин на 4 ядрах (одноразово); пересборка std после правок pal —
~1 мин. Программы: `rust-toolchain.toml` с `channel = "void"` рядом с крейтом
(ближний файл побеждает Code/stable) — и обычный `cargo build` в nix-shell.

## Грабли (все — по одному разу)

- **Сабмодуль llvm-project не нужен вовсе**: внешний LLVM из nix + `lld = false`;
  но rust-lld тогда взять неоткуда → симлинк из rustup, и его **стирает каждый
  прогон x.py** (sysroot пересобирается) — пересоздавать.
- **Sysroot собирается только для таргетов текущего вызова** — riscv-std исчезла
  после отдельного x86-прогона; все три таргета строить ОДНИМ вызовом x.py.
- **memcpy/memset undefined при линковке** — таргет без libc обязан включать
  `compiler-builtins-mem` (правка в bootstrap/lib.rs, узнали по zkvm).
- **thread_local**: два диспетчера — storage (no_threads ✓ списком) и guard
  (`_ =>` тянет os-ключи TLS) — guard требует явной ветки с пустым `enable()`.
- **strict provenance** в std: `usize as *mut u8` запрещён —
  `ptr::with_exposed_provenance_mut` (адрес пришёл из asm-регистра SYS_MAP).
- **`_ =>` дефолты не всегда unsupported**: io/error по умолчанию не существует
  (cfg_select без веток — ошибка), exit по умолчанию — `intrinsics::abort()`
  (гибель вместо кода выхода!) — обе ветки добавлены явно.
- build-скрипты — под хост-триплет: cc есть только в nix-shell (грабля Вехи 29).

## Что теперь под ядром/README

Ядро подросло под std: стек процесса 4→16 страниц (fmt/sort прожорливее),
куча ядра 2→8 МиБ (std-ELF в куче дважды: кэш store + копия загрузчика).
Пик RAM: ~4.9 → **~12.2 МиБ** (в основном резерв кучи) — честно обновлено в README.

## Ограничения (осознанные, до потребителей)

~~Аллокатор течёт (bump) · fs unsupported · SystemTime нет · float на riscv —
мина (sstatus.FS)~~ — всё четыре сняты Вехой 32 ([[uutils]]): free-list
аллокатор, std::fs по IPC к posixfs, SystemTime с фиктивной базой
(2026-07-01 + uptime), FP-контекст в ядре. Остались: process/net
unsupported · потоков нет (sort/rayon/ripgrep ждут) · random слабый.

## Связано
- [[0004-void-pkg]] · [[void-pkg]] — дорожка; [[process-contract]] — контракт, на который порт опирается
- [[store-bridge]] — доставка бинарей; [[elf-userspace]] — загрузчик и линкер-скрипт
