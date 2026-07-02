---
title: Веха 13 — сервер объектного store в userspace
created: 2026-07-02
tags: [project/void, topic/kernel, topic/ipc, topic/processes, topic/capabilities, topic/objects, lang/rust, note]
status: active
---

# Веха 13 — сервер объектного store в userspace

Персистентное [[object-model|пространство объектов]] ([[persistent-store]]) отдаётся как **сервис
в непривилегированном процессе**, а не зашивается в каждый процесс. Клиент, имея лишь cap на
[[ipc-endpoints|эндпоинт]] сервера, кладёт и читает объекты по IPC — и ни разу не касается store
напрямую. Это прямой шаг по [[0002-persistent-content-addressed-capability-core|ADR 0002]]:
единое пространство объектов доступно через capability, а не через привилегию, зашитую в ядро.

Архитектура — та же, что у [[block-driver|драйвера блоков]]: тонкий userspace-сервер поверх
привилегированного шлюза ядра, доступ к которому закрыт capability.

## Что добавлено
1. **Передача буфера запроса клиент→сервер** (зеркало `REPLY` из [[block-driver|Вехи 11]]).
   - `CALL(ep_cap, op, send_buf, send_len, recv_buf, recv_cap)` — теперь несёт и **буфер
     запроса**, и приёмный буфер ответа.
   - `RECV(recv_buf, recv_cap) -> (op, from, len)` — приняв запрос, ядро копирует его нагрузку из
     буфера клиента в буфер сервера. Копирование **между двумя адресными пространствами** —
     `copy_between_spaces`: оба конца транслируются постранично в физические адреса (RAM
     идентично отображена в ядре, `satp` переключать не нужно). Клиент в этот момент в
     `ReplyWait` — его память стабильна.
2. **Store как ресурс-capability** (`cap::Target::Store`): `READ` — читать значения (`OBJ_GET`),
   `WRITE` — класть новые (`OBJ_PUT`). Соединяет [[capabilities]] с объектной моделью.
3. **Привилегированные шлюзы к store** (guarded by store-cap):
   - `OBJ_PUT(store_cap, buf, len, id_out)` — сохранить значение, вернуть 32-байтный content-id;
   - `OBJ_GET(store_cap, id_ptr, out, cap) -> len` — прочитать значение по content-id.
   Буферы читаются/пишутся в пространстве вызывающего (он `current`, `SUM=1`).
4. **`proc::run` стартует с первого готового процесса** (не индекс 0) — чтобы работали
   ПОСЛЕДОВАТЕЛЬНЫЕ сессии: после сессии драйвера её процессы остаются Finished/заблокированными,
   а новая сессия store (P2/P3) стартует со своего первого Runnable.

## Сервер и клиент (U-mode)
- **Сервер store** (P2, cap на store `rw--`): `loop { (op,from,len)=RECV(req); match op { PUT =>
  OBJ_PUT(req,len)->id; REPLY(id,32), GET => OBJ_GET(req)->val; REPLY(val,len) } }`.
- **Клиент** (P3, cap только на эндпоинт `---s`): `put` сообщения → получает content-id → `get`
  по нему → печатает значение. Затем пытается `OBJ_PUT` НАПРЯМУЮ (эндпоинт-cap, не store) → отказ.

## Нюанс, который поймали: переполнение trap-стека
Первый прогон упал в `store page fault` внутри `blake3::compress_in_place`. Причина: ядерный
trap-стек был 16 КиБ, а прямо под ним в памяти лежит секция `.user` (`R|X`, **не writable**).
Теперь syscall делает НАСТОЯЩУЮ работу (`object::put` → BLAKE3 + куча + `println!`), и в
debug-сборке с её крупными кадрами стек уходил вниз в `.user` → фатальная запись. Увеличили
trap-стек до **64 КиБ** (как загрузочный стек ядра в `linker.ld`). Раньше `object::put` работал
только на большом загрузочном стеке — на маленьком trap-стеке впервые.

## Проверка (вывод в QEMU)
```
[proc] userspace-сервер объектного store + клиент через IPC:
  P2 'obj-store' ← cap на store [rw--]
  P3 'store-cli' ← cap на эндпоинт P2 [---s]
[ipc] P3 CALL P2 (по cap) op=0 (35 байт)
[obj] P2 OBJ_PUT 35 байт → content-id (по cap)
[ipc] P2 REPLY P3 (32 байт)
[ipc] P3 CALL P2 (по cap) op=1 (32 байт)
[obj] P2 OBJ_GET → 35 байт (по cap)
[ipc] P2 REPLY P3 (35 байт)
[store-cli] get by content-id -> hello from VOID object-store client
[obj] P3 OBJ_PUT отклонён: Denied  ← нет capability на store
[store-cli] direct OBJ_PUT DENIED by kernel (no store capability)
```
Данные прошли путь **клиент → IPC → сервер-процесс → ядерный store → IPC → клиент**, круговой
put/get подтверждён по content-id. Прямой доступ клиента к store отвергается. Стабильно через
перезагрузки; персистентность прежнего состояния не задета (поколение растёт как обычно).

## Осознанные упрощения (следующее)
- **Store всё ещё через ядерный шлюз** (`OBJ_PUT`/`OBJ_GET`), а не сам живёт в процессе. Полностью
  userspace-store (сервер владеет диском и объектным форматом) — большой отдельный шаг.
- ~~**Нет `SET_ROOT`/именованных корней по IPC**~~ — **закрыто на [[persistent-root-ipc|Вехе 14]]**:
  `OBJ_SET_ROOT`/`OBJ_GET_ROOT` под store-cap; объект, привязанный к корню, переживает перезагрузку.
- **`op` — скаляр, один буфер запроса/ответа** (≤512 Б). Полноценный протокол (несколько
  аргументов, стрим) — позже.
- **Кооперативно, один сервер/клиент**; c-space процессов в RAM (как на [[capabilities|Вехе 8]]).

## Что дальше
- Право на **именованный корень** как capability (put + set_root по IPC → объект переживает
  перезагрузку через сервер).
- Позже: полностью userspace-store; reply-capability; `grant` прав между процессами через IPC.
- Ещё позже: **Веха 14** — слой совместимости (Linux/POSIX-персоналия) как сервер.
