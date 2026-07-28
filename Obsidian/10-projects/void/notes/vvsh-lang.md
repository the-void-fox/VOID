---
title: vvsh — язык (крейт vvsh-core): reader + вычислитель + нормализатор конфига
created: 2026-07-01
tags: [project/void, topic/vvsh, topic/language, topic/config]
status: active
---

# vvsh — язык: reader, вычислитель, нормализатор (крейт `vvsh-core`)

Реализация маленького гомоиконного Lisp'а VOID из [[0006-vvsh-lisp-config-shell]]. Живёт в
`Code/libs/vvsh-core` — `no_std`+`alloc`, без зависимостей, host-тестируемый. Модель раскладки
конфига («FS-источник + поколение-сборка») — в [[vvsh-config-layout]].

## Веха 75 — M1a: конфиг ВЫЧИСЛЯЕТСЯ на VOID (reader → eval → нормализация)

Первая точка: `.vv`-файл читается из posixfs, вычисляется **на самом VOID** и печатается
нормализованным системным конфигом (те же строки `service …`/`shell …`, что даёт `nix/system.nix`
и читает `init.rs::apply`). Хостовый Nix для этого больше не нужен — язык самохостится.

### Крейт `vvsh-core` (модули)
- **`value.rs`** — `Value` (Bool/Int/Str/Sym/List/Builtin/Closure; списки — proper list на `Vec`,
  не cons; строки/списки/замыкания под `Rc` — дёшев клон), окружение `Env` (цепочка
  `Rc<RefCell<Scope>>`, лексический скоуп), `EvalError`. Каноничная печать (`Display`) — основа
  модели «значение → объект store».
- **`reader.rs`** — `read_all(&str) -> Vec<Value>`: списки `(…)`, строки `"…"` (escape `\n \t \" \\`),
  целые `-?[0-9]+`, `#t`/`#f`, символы, `;`-комментарии, `'x`→`(quote x)`.
- **`eval.rs`** — метациркулярный `eval` + спец-формы `quote if define lambda let cond begin and or`
  (+ сахар `(define (f a) …)`); встроенные `list append cons car cdr null? not = + - * service shell
  system`. Истинность по Scheme (ложь только `#f`).
- **`config.rs`** — `normalize_config`: `(#system (kind имя право…)…)` → построчный текст init'а.
- **`lib.rs`** — `build_config(&str) -> Result<String,String>` (весь конвейер) + 7 host-тестов.

`service`/`shell` — встроенные-конструкторы: имя + права (строки или список строк «вливается», чтобы
компоновать через `append`/`if`). `system` уплощает записи/списки записей в верхнюю форму `#system`.

### Программа `bin/<arch>/vvsh`
`run vvsh eval FILE` — читает `.vv` из posixfs (`start_cap(0)` = posixfs-endpoint, наследуется от vsh
как в install.rs; `stat` до `open`, чтобы опечатка не плодила пустышку), зовёт `build_config`, печатает.
Свой `#[global_allocator]` — bump поверх `heap_map` (короткоживущий процесс: eval→печать→exit, dealloc
no-op). `vvsh-core` линкуется ТОЛЬКО в этот бинарь.

### Проверка (QEMU, обе арх — x86 q35 и riscv-virt)
Конфиги одной строкой (S-выражения без переносов влезают в 128-байт буфер vsh):
- `(system (service "posixfs" "store:rw") (shell "vsh" "endpoint:posixfs" "env"))` →
  `service posixfs store:rw` / `shell vsh endpoint:posixfs env` (точный формат, две записи);
- `(define net #t)(system (if net (service "net-srv" "dev:net:rw") (list)))` → `service net-srv dev:net:rw`
  (ветка **then**);
- `(define n #f)(system (if n (service "a" …) (service "b" …)))` → `service b store:rw` (ветка **else**
  — доказывает, что `if` реально ВЫЧИСЛЯЕТСЯ, а не эхо);
- несуществующий путь → «не удалось прочитать файл» (код 1, пустышку не создаём); `run vvsh` → справка.

Полную эквивалентность gen1/gen2 (с `net #t`/`net #f`) доказывают host-тесты байт-в-байт
(`normalizes_full_config`, `net_off_matches_gen2`) — без QEMU. Обе арх: 0 предупреждений. Ядро VOID
не менялось (A2 — язык в userspace).

### Границы M1a (дальше)
- **M1b** — `(import "networking.vv")` + слияние по `/etc/system/*.vv`.
- **M1c** — `rebuild` (eval → коммит поколения), сев `/etc/system/*.vv`, загрузка ядром из снимка,
  команда `gens`. Тогда — полная эквивалентность на устройстве и уход от хостового Nix для конфига.
- Диалект: макросы/quasiquote — отложены; числовая башня — только i64.

## Связано
- [[0006-vvsh-lisp-config-shell]] (ADR) · [[vvsh-config-layout]] (раскладка/bootstrap/вехи) ·
  [[vvsh-bidirectional-config]] (overlay/promote — поверх этого) · [[declarative-init]] (формат норм.
  данных, куда целимся) · [[void-dev-env-workflow]] (сборка/QEMU).
