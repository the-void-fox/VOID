---
title: Веха 14 — именованный корень как capability (persistence по IPC)
created: 2026-07-02
tags: [project/void, topic/kernel, topic/ipc, topic/capabilities, topic/objects, topic/persistence, lang/rust, note]
status: active
---

# Веха 14 — именованный корень как capability

Замыкает «ОС не забывает» ([[persistent-store]]) с userspace-доступом: значение, положенное
клиентом через [[object-store-server|сервер store]], можно **привязать к именованному корню** и
на следующем запуске прочитать обратно — всё по IPC под [[capabilities|capability]], без прямого
доступа к пространству объектов. Закрывает упрощение [[object-store-server|Вехи 13]] «нет
`SET_ROOT`/именованных корней по IPC».

## Что добавлено
1. **Шлюзы корней под store-cap:**
   - `OBJ_SET_ROOT(store_cap, name, name_len, id_ptr)` — привязать корень `name` к content-id
     (нужен `WRITE`); имя читается как UTF-8 из буфера вызывающего, `object::set_root` хранит его
     как `String` и коммитит вместе с объектами (переживает перезагрузку).
   - `OBJ_GET_ROOT(store_cap, name, name_len, id_out) -> 32/0/MAX` — content-id корня или 0, если
     корня нет (нужен `READ`).
2. **Протокол сервера store расширен до 4 операций** (`op`): `PUT`, `GET`, `SET_ROOT`, `GET_ROOT`.
   Для `SET_ROOT` клиент шлёт буфер `[id(32) | name]`; сервер отдаёт ядру `name = req+32`,
   `id = req`.

## Нюанс, который поймали: данные U-mode должны быть U-читаемы
Первый прогон упал в U-mode с `scause=0xd` (load page fault). Клиент сам, **в U-mode**, собирал
буфер запроса `[id | name]`, читая имя из статика `GREETING`. Раньше статики (`MSG`, префиксы)
всегда лишь **передавались указателем ядру**, а читало их ядро (S-mode, `SUM=1`) — поэтому `.rodata`
(смаплена `R`, без флага `U`) годилась. Здесь же строку читает **сам процесс**, и `.rodata` ему
недоступна. Исправление: `GREETING` помечен `#[link_section = ".user"]` — попадает в U|R|X-страницу
`.user`, доступную из U-mode. Общий принцип: **любые данные, которые процесс читает сам, должны
лежать в U-доступной секции**, а не только передаваться ядру по указателю.

## Проверка (два запуска подряд)
```
── запуск 1 (корня ещё нет) ──
[obj] P2 OBJ_GET_ROOT 'greeting' → нет (по cap)
[store-cli] root 'greeting' not set yet (first boot)
[obj] P2 OBJ_PUT 35 байт → content-id (по cap)
[obj] P2 OBJ_SET_ROOT 'greeting' (по cap)
[store-cli] value stored and bound to root 'greeting' (survives reboot)

── запуск 2 (после перезагрузки) ──
[obj] P2 OBJ_GET_ROOT 'greeting' → есть (по cap)
[obj] P2 OBJ_GET → 35 байт (по cap)
[store-cli] root 'greeting' from previous boot: hello from VOID object-store client
```
Значение пережило перезагрузку и прочитано обратно **целиком через userspace-сервер**: клиент ни
разу не касался ни диска, ни объектного пространства напрямую. Прямой `OBJ_PUT` эндпоинт-cap'ом —
по-прежнему `Denied`.

## Осознанные упрощения (следующее)
- **Store/драйвер всё ещё через ядерные шлюзы**, а не живут в процессах целиком (MMIO/DMA/формат
  на диске) — крупный отдельный шаг.
- **C-space процессов в RAM** — при перезагрузке права минтятся заново ядром (bootstrap); сами
  capability пока не персистентны ([[capabilities]]).
- **Имя корня — произвольный UTF-8**, но нет прав на *конкретный* корень (cap на store даёт все
  корни). Гранулярность «cap на корень X» — позже.

## Что дальше
- [[reply-capability|reply-capability]]: защитить `REPLY` (нельзя ответить тому, кто не звал).
- Вытеснение процессов (таймер в U-mode).
- Позже: полностью userspace-store/драйвер; персистентный c-space; `grant` прав через IPC.
