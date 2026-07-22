---
title: Веха 39 — WASI. wasm-модули через wasmi в userspace
created: 2026-07-01
tags: [project/void, topic/packages, topic/wasm, topic/wasi]
status: done
---

# Веха 39 — WASI (бэкенд C)

Бэкенд **C** пакетной дорожки ([[0004-void-pkg]], [[void-pkg]]) — последний из трёх:
неизменённые **wasm32-wasi**-модули работают на VOID через интерпретатор **wasmi**.
Универсальный медленный fallback: один и тот же .wasm идёт на **обеих** архитектурах
без пересборки — интерпретатор арх-нейтрален, а cap-модель WASI (preopen-дескрипторы)
идейно ложится на нашу ([[capabilities]]).

## Микроядерно чисто: раннер в userspace

В отличие от бэкенда B (транслятор Linux-syscall'ов В ЯДРЕ, [[linux-abi]]), здесь
ядро **не тронуто вовсе**. `bin/wasirun` — ОБЫЧНАЯ std-программа (тулчейн `void`,
как [[std-port|std-hello]]), тянущая крейт `wasmi` (0.36, no_std-режим). Она:

- читает .wasm как обычный файл — `std::fs::read` → IPC к персоналии posixfs
  (стартовая capability, слот 0);
- инстанцирует модуль в wasmi, определяет импорты `wasi_snapshot_preview1`;
- зовёт `_start`.

WASI-импорты замыкаются на std-порт: `fd_write` → `println!`/stderr → SYS_WRITE,
`proc_exit(code)` → `std::process::exit` → SYS_EXIT, `args_{sizes_get,get}` — из
`std::env::args` (argv гостя = имя модуля + доп. аргументы `run`). Реализованы те
6 импортов, которых требует Rust/C hello: args_{sizes_get,get},
environ_{sizes_get,get}, fd_write, proc_exit.

## Что заработало

`run bin/wasirun hello.wasm [аргументы…]` печатает из wasm-гостя, читает argv,
выходит с кодом — **тот же .wasm на riscv64 и x86_64**. Модуль — обычный
`cargo build --target wasm32-wasip1` Rust-hello (63 КБ), доставлен мостом в
персоналию как файл `hello.wasm`.

## Две подпорки, которых потребовал wasmi

- **posixfs: файл 4 КиБ → 128 КиБ.** wasm-модуль (63 КБ) не влезал в прежний
  потолок персоналии. Подняли `DATA_MAX` (буферы файлов — в ленивой куче, платим
  лишь за записанное); индекс каталога вынесли в свой скромный `DIR_MAX` (он на
  стеке — 128 КиБ там дали бы переполнение).
- **std-арена 16 МиБ → 64 МиБ.** Интерпретатору тесно в 16 (парсинг модуля +
  линейная память гостя + буферы = аллокации в единицы МиБ). Правка одной
  константы в порту std (`sys/alloc/void.rs`), пересборка std ~5 мин + пересоздать
  симлинк rust-lld ([[std-port]]). Резерв ленив — нетронутый хвост фреймов не стоит.

## Грабли

- Свежий диск после интенсивного тестирования: чередование крупных мостовых
  `put` (wasirun 2 МБ) и загрузок ядра с уплотнением store оставило индекс в
  состоянии «0 объектов» для моста. Лечится пересборкой диска (артефакт gitignore):
  `truncate` → загрузка ядра пересевает `bin/<arch>/*` → мост доставляет остальное.
- wasmi-замыкание `proc_exit` возвращает `!` → never-type fallback: аннотировать
  `-> ()`.

## Три бэкенда — закрыты

Пакетная дорожка ADR [[0004-void-pkg]] пройдена целиком: **A** — native/nix-cross
([[nixpkgs-cross]], [[uutils]]), **B** — linux-abi ([[linux-abi]]), **C** — wasi
(эта веха). `run` в vsh запускает четыре мира с одного диска: родной ELF VOID,
static-PIE musl (Linux), busybox, wasm.

## Связано
- [[0004-void-pkg]] · [[void-pkg]] · [[linux-abi]] (бэкенд B) · [[nixpkgs-cross]] (бэкенд A)
- [[std-port]] (тулчейн void, арена аллокатора) · [[posix-personality]] · [[capabilities]]
