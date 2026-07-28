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

## Веха 76 — M1b: `import` + слияние модулей (`/etc/system/*.vv`)

Конфиг стал **модульным**: `default.vv` собирает систему из отдельных `.vv`, каждый возвращает
свой ВКЛАД, а слияние — обычным `append` (список-конкат). Ровно `imports = [ … ]` NixOS, но нативно.

**Инъекция I/O (ключевое):** `import` читает файлы, а `vvsh-core` — чистый, без I/O. Поэтому ввёл
trait `ModuleLoader { load(name) -> Result<String,String> }`; `import` — спец-форма, берёт исходник
у загрузчика. Бинарь `vvsh` даёт `FsLoader` поверх posixfs (имя резолвится относительно каталога
корневого файла: `import "services.vv"` из `/etc/system/default.vv` → `/etc/system/services.vv`;
имя с `/` — абсолютно). Host-тесты — `MapLoader` из карты в памяти. Крейт остаётся без I/O.

Рефактор: `eval` и спец-формы стали методами `Interp` (несёт `loader` + кэш импортов по имени +
стек загрузки). Модуль вычисляется в СВЕЖЕМ окружении (не видит define'ов импортёра) и через тот же
`Interp` (вложенные import'ы делят кэш). **Кэш** — модуль грузится раз; **стек** — детект циклов.
API: `build_config_with(src, &loader)` (M1b), `build_config(src)` = без загрузчика (M1a, import→ошибка).

Тесты (13 всего): `imports_and_merges_to_gen1` (четыре `.vv` → gen1 ТОЧНО), `module_can_compute_its_
contribution` (define/if внутри модуля), `import_is_cached` (2 импорта → 1 загрузка), `import_cycle_
detected` (a↔b → ошибка, не зависание), `import_missing_errors`, `import_without_loader_errors`.

Проверка (QEMU, x86 + riscv): `/etc/system/{services,networking,shell}.vv` + `default.vv` с тремя
`import` внутри `(append …)` → `run vvsh eval /etc/system/default.vv` печатает ТОЧНО gen1 (три
строки), причём `networking.vv` сам вычислил вклад через `define`/`if`. Импорт отсутствующего модуля
→ `vvsh: ошибка: модуль 'nope.vv' не найден` (код 1). Обе арх 0 предупреждений, ядро не менялось.

## Веха 77 — M1c: `rebuild` (eval → поколение) + сев `/etc/system` + `gens` — фаза КОНФИГА закрыта

Замкнут полный цикл самохостинга конфига на устройстве (модель [[vvsh-config-layout]]: FS-источник →
поколение-сборка → бут из снимка). Правишь `/etc/system/*.vv` на VOID → `rebuild` на VOID → ребут
грузит пересобранный конфиг. **Хостовый Nix для системного конфига больше не нужен.**

Подкоманды `vvsh` (обвязка store — start-cap 1):
- **`init-config`** — сеет модульный конфиг `/etc/system/{net,services,networking,default}.vv` (через
  posixfs). Тумблер сети — в `net.vv` (`#t`/`#f`), `default.vv` = `(define net (import "net.vv"))` +
  сборка через `append`+`if` (net #t → gen1-конфиг, net #f → gen2-конфиг).
- **`rebuild`** — вычисляет `/etc/system/default.vv` → нормализованный текст → `obj_put` (контент-
  адресуемо) → корень `system/gen<N>` (N = max существующих +1) → двигает `system/current`. **Dedup:**
  если содержимое == текущему поколению → «нет изменений» (не плодим). Активно после ребута.
- **`gens`** — перечислить `system/gen*` (сортировка) + пометить активное `*`.
- vsh: тонкие встроенные `rebuild`/`gens` (обёртки над `run vvsh …`).

**Права store (решение):** `OBJ_PUT`/`SET_ROOT`/`LIST_ROOTS` требуют WRITE (есть у shell'а), а
`OBJ_GET`/`GET_ROOT` (dedup, чтение current для маркера `*`) — READ. Дал админ-шеллу READ:
`store:xw` → **`store:rwx`** (init.rs `DEFAULT_GEN1/2`, nix/system.nix, сеянный `default.vv`) — чтение
поколений это работа шелла, а WRITE у него уже мощнее. Код ДЕГРАДИРУЕТ мягко: нет READ (старый диск с
`xw`) → просто без dedup/маркера, без падений. Полный функционал — на свеже-сеянных/пересобранных
системах.

Бут не менялся (`init.rs::boot` читает нормализованный снимок `system/<gen>`); eval — только на
`rebuild`. Поколение = контент-адресный кэш выхода eval (как деривация).

Проверка (QEMU, x86 q35 + riscv-virt, СВЕЖИЙ диск — сеется `rwx`): `init-config` → `gens` (gen1\*,
gen2) → `rebuild` (net #t → gen3, «было gen1») → `gens` (gen3\*) → `rebuild` снова → **«нет изменений
— уже gen3» (dedup)** → `echo #f > /etc/system/net.vv` → `rebuild` (net #f → gen4) → `gens` (gen4\*).
**Ребут** того же диска: `[init] поколение 'gen4'`, стартуют только `posixfs`+`vsh` (net-srv нет),
`ping` → «сети нет» — система поднялась в ПЕРЕСОБРАННОЙ конфигурации. Обе арх 0 предупреждений.

### Дальше — фаза ШЕЛЛА (пруф №2)
Половина конфига (M1) закрыта. Дальше vvsh как интерактивный шелл/REPL (конвейеры/переменные поверх
того же языка). Диалект: макросы/quasiquote — отложены; числовая башня — только i64.

## Связано
- [[0006-vvsh-lisp-config-shell]] (ADR) · [[vvsh-config-layout]] (раскладка/bootstrap/вехи) ·
  [[vvsh-bidirectional-config]] (overlay/promote — поверх этого) · [[declarative-init]] (формат норм.
  данных, куда целимся) · [[void-dev-env-workflow]] (сборка/QEMU).
