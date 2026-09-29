//! Веха 214.6 — системные вызовы: **каналы и права**.
//!
//! Запрос-ответ через эндпоинты, одноразовые reply-права, аттенуация копии и справка о праве.
//! Самая сердцевина capability-модели: всё, чем права ходят между процессами.
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
    match num {
        // SYS_RECV(recv_buf, recv_cap, nonblock) -> (a0=op, a1=reply-право, a2=длина запроса,
        // a3=право из сообщения, a4=НОМЕР ОТПРАВИТЕЛЯ — Веха 99). Комментарий выше долго
        // утверждал, что отправитель в a1, а там всегда было reply-право; теперь отправитель
        // есть на самом деле. Он нужен серверу, который ведёт по клиенту СОСТОЯНИЕ: мультиплексор
        // обязан понять, в какую панель лёг вывод, а reply-право для этого не годится — оно
        // одноразовое и у каждого запроса своё.
        // a3=принятое право|MAX — Веха 21.1). Приняв запрос, копируем его полезную нагрузку из
        // буфера клиента в recv_buf; если клиент передал capability — она уже скопирована в домен
        // сервера (deliver_request), в a3 — её дескриптор.
        //
        // Нет запроса, три режима (arg2): 0 — блокировка (RecvWait), recv_buf/cap сохранены, чтобы
        // доставка позже скопировала в них; 1 — немедленный возврат с `op == usize::MAX`
        // (Веха 90); 2 — блокировка ДО ДЕДЛАЙНА (arg3, тики), Веха 91: возврат с `op == MAX`,
        // когда время вышло. Третий режим и есть «сон вместо опроса» для реактора: он спит,
        // пока не придёт запрос или не настанет момент, который назвал сам стек (`poll_at`).
        //
        // Зачем неблокирующий приём. Сетевому серверу нужно ОДНОВРЕМЕННО прокачивать стек
        // (входящие кадры, таймеры ретрансмиссии) и отвечать клиентам. Пока `SYS_RECV` умел
        // только блокировать, сервер стоял в нём и стек не тикал. Альтернативой были две нити с
        // общим состоянием стека под мьютексом — но smoltcp насквозь `&mut`, и такой мьютекс
        // сериализовал бы всё равно всё, добавив лишь способы ошибиться. Один реактор проще и
        // честнее; ждать СРАЗУ кадра и IPC-сообщения (вместо опроса) научит Веха 91.
        4 => {
            let (rbuf, rcap, mode, timeout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            t.procs[cur].recv_buf = rbuf;
            t.procs[cur].recv_cap = rcap;
            if let Some(pos) = t.mailbox.iter().position(|&(_, to, _)| to == cur) {
                let (from, _to, op) = t.mailbox.remove(pos);
                let (n, tcap) = deliver_request(t, from, cur);
                // Веха 101 — сколько байт запроса ДОШЛО, узнаёт отправитель (третье значение его
                // `SYS_CALL`). Он ещё спит в ReplyWait; ответ выставит a0/a1 и двинет sepc, a2
                // при этом сохранится.
                t.procs[from].frame.set_ret_at(2, n);
                let rc = cap::mint(t.procs[cur].domain, cap::Target::Reply(from), Rights::SEND);
                let f = &mut t.procs[cur].frame;
                f.set_ret(op);
                f.set_ret_at(1, rc.bits() as usize);
                f.set_ret_at(2, n);
                f.set_ret_at(3, tcap);
                f.set_ret_at(4, from);
                f.advance();
            } else if mode == 1 {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX); // «запросов нет» — вызывающий занимается своими делами
                f.set_ret_at(2, 0);
                f.advance();
            } else {
                // Веха 91: дедлайн живёт в том же поле, что у futex - механика пробуждения по
                // времени уже есть (`wake_futex_timeouts`), заводить вторую незачем.
                t.procs[cur].futex_deadline =
                    (mode >= 2).then(|| arch::now_ticks().wrapping_add(timeout as u64));
                // Режим 3: разбудить ещё и приходом кадра - тогда сервер реагирует на сеть
                // мгновенно, а не на ближайшем тике таймера.
                t.procs[cur].wake_on_net = mode == 3;
                // Веха 103 — режим 4: разбудить и по клавише (реактор терминала).
                t.procs[cur].wake_on_key = mode == 4;
                t.procs[cur].state = State::RecvWait; // sepc не двигаем: доставка сделает это
                if let Some(n) = t.next_runnable(cur) {
                    t.set_cur(n);
                }
            }
        }
        // SYS_CALL(ep_cap, op, send_buf, send_len, recv_buf, recv_cap, a6=cap|MAX) ->
        // (a0 = число байт ответа | MAX, a1 = право из ответа | MAX). `ep_cap` — дескриптор
        // эндпоинта в c-space процесса; ядро резолвит его в id сервера. `send_buf`/`send_len` —
        // полезная нагрузка запроса (копируется серверу при доставке). Веха 21.1: `a6` —
        // capability, передаваемая в сообщении (нужен `GRANT` на неё — проверяется ЗДЕСЬ,
        // до отправки); сервер получит её копию в своём домене (a3 его RECV). Ответ сервера
        // тоже может нести право — его дескриптор вернётся в a1. Отправить и ждать (блокируется).
        5 => {
            let (ecap, op, sbuf, slen, rbuf, rcap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let scap = t.procs[cur].frame.arg(6); // право в сообщении (MAX — нет)
            let dom = t.procs[cur].domain;
            // Передаваемое право проверяем ДО отправки: нет GRANT — весь CALL отклонён.
            if scap != usize::MAX {
                let ok = cap::rights(dom, Cap::from_bits(scap as u64))
                    .map_or(false, |r| r.contains(Rights::GRANT));
                if !ok {
                    vprintln!("  [cap] P{} CALL отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return true;
                }
            }
            match cap::endpoint(dom, Cap::from_bits(ecap as u64)) {
                Ok(dest) => {
                    // Веха 153.3 — учёт IPC для диспетчера: клиент сделал вызов, сервер принял.
                    // Считаем при ПРИЁМЕ вызова (а не при доставке), чтобы и отложенный в mailbox
                    // счёлся: адресат уже назначен, а «сделал CALL» — факт со стороны клиента.
                    t.procs[cur].calls_made = t.procs[cur].calls_made.wrapping_add(1);
                    t.procs[cur].bytes_sent = t.procs[cur].bytes_sent.wrapping_add(slen as u64);
                    if dest < t.procs.len() {
                        t.procs[dest].calls_recv = t.procs[dest].calls_recv.wrapping_add(1);
                        t.procs[dest].bytes_recv = t.procs[dest].bytes_recv.wrapping_add(slen as u64);
                    }
                    vprintln!("  [ipc] P{} CALL P{} (по cap) op={} ({} байт)", cur, dest, op, slen);
                    t.procs[cur].recv_buf = rbuf;
                    t.procs[cur].recv_cap = rcap;
                    t.procs[cur].send_buf = sbuf;
                    t.procs[cur].send_len = slen;
                    t.procs[cur].send_cap = scap;
                    if dest < t.procs.len() && t.procs[dest].state == State::RecvWait {
                        // получатель ждёт в RECV — доставить нагрузку в его буфер и разбудить.
                        // Выдать серверу одноразовый reply-cap на этого клиента (см. [[reply-capability]]).
                        let (n, tcap) = deliver_request(t, cur, dest);
                        t.procs[cur].frame.set_ret_at(2, n); // Веха 101: доставлено байт запроса
                        let rc = cap::mint(t.procs[dest].domain, cap::Target::Reply(cur), Rights::SEND);
                        let df = &mut t.procs[dest].frame;
                        df.set_ret(op);
                        df.set_ret_at(1, rc.bits() as usize);
                        df.set_ret_at(2, n);
                        df.set_ret_at(3, tcap);
                        df.set_ret_at(4, cur);
                        df.advance();
                        t.procs[dest].state = State::Runnable;
                        t.procs[dest].ready_at = arch::now_ticks();
                    } else {
                        t.mailbox.push((cur, dest, op)); // нагрузку скопируют при его RECV
                    }
                    t.procs[cur].state = State::ReplyWait; // sepc двинет доставка ответа
                    if let Some(n) = t.next_runnable(cur) {
                        t.set_cur(n);
                    }
                }
                Err(e) => {
                    // Нет валидного cap на эндпоинт — отказ. Процесс не блокируется, продолжает.
                    vprintln!("  [ipc] P{} CALL отклонён: {:?}  ← нет capability на эндпоинт", cur, e);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                }
            }
        }
        // SYS_REPLY(reply_cap, src_buf, len, a3=cap|MAX) -> 0/MAX: ответить вызвавшему клиенту,
        // передав `len` байт из своего буфера в его приёмный буфер, и разбудить его. `reply_cap` —
        // одноразовый cap на клиента, выданный при `RECV`; ядро резолвит его в id клиента и по
        // исполнении отзывает. Подделать/переиспользовать нельзя (см. [[reply-capability]]).
        // Веха 21.1: `a3` — право, передаваемое С ОТВЕТОМ (нужен `GRANT`); клиент получит его
        // дескриптор в a1 своего CALL. Паттерн «сервер-раздатчик»: клиент просит доступ,
        // сервер отвечает УРЕЗАННОЙ копией своего права (CAP_DERIVE → REPLY).
        //
        // Веха 155: `a4` — МАСКА прав на передаваемое право (0 — «как есть», как было до вехи).
        // Без неё «урезанной копии» из абзаца выше не получалось: передача требует `GRANT`, а
        // копия ехала маской `MAX`, то есть С ЭТИМ ЖЕ `GRANT`. Раздатчик не мог отдать право,
        // которое нельзя раздать дальше, — а именно так композитор отдаёт диспетчеру обзор
        // процессов ([[task-manager]]): смотреть и отзывать — да, вручать третьим — нет.
        6 => {
            let (rcap, src, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let scap = t.procs[cur].frame.arg(3); // право в ответе (MAX — нет)
            let smask = t.procs[cur].frame.arg(4); // маска прав на него (0 — как есть)
            let dom = t.procs[cur].domain;
            // Как в CALL: передаваемое право проверяем до доставки — нет GRANT, нет REPLY.
            if scap != usize::MAX {
                let ok = cap::rights(dom, Cap::from_bits(scap as u64))
                    .map_or(false, |r| r.contains(Rights::GRANT));
                if !ok {
                    vprintln!("  [cap] P{} REPLY отклонён: нет права GRANT на передаваемую capability", cur);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                    return true;
                }
            }
            let result = match cap::reply_endpoint(dom, Cap::from_bits(rcap as u64)) {
                Ok(dest) => {
                    vprintln!("  [ipc] P{} REPLY P{} ({} байт)", cur, dest, len);
                    if dest < t.procs.len() && t.procs[dest].state == State::ReplyWait {
                        let mut n = len.min(t.procs[dest].recv_cap);
                        // Веха 23: оба конца могут лежать в ленивых кучах — доотобразить: свой
                        // буфер ядро читает напрямую (S-фолт фатален), приёмник клиента
                        // транслируется постранично (немапленное молча пропало бы).
                        if n > 0
                            && !(ensure_heap_range(t, cur, src, n)
                                && ensure_heap_range(t, dest, t.procs[dest].recv_buf, n))
                        {
                            n = 0; // фреймы кончились — честнее не доставить ничего
                        }
                        // Читаем из текущего (сервера) по SUM=1; пишем в пространство клиента через
                        // трансляцию его таблицы (физ. адрес отображён в ядре идентично).
                        // Пустой ответ (n=0, напр. только право — Веха 21) не строит слайс:
                        // from_raw_parts из нулевого указателя — UB даже при нулевой длине.
                        if n > 0 {
                            let src_slice = unsafe { core::slice::from_raw_parts(src as *const u8, n) };
                            let droot = arch::space_root(t.procs[dest].space);
                            let dbuf = t.procs[dest].recv_buf;
                            copy_to_space(droot, dbuf, src_slice);
                        }
                        // Право в ответе: скопировать в домен клиента; его дескриптор — в a1 CALL.
                        let mut tcap = usize::MAX;
                        if scap != usize::MAX {
                            if let Ok(nc) = cap::grant(
                                dom,
                                Cap::from_bits(scap as u64),
                                t.procs[dest].domain,
                                if smask == 0 { Rights(u32::MAX) } else { Rights(smask as u32) },
                            ) {
                                tcap = nc.bits() as usize;
                                vprintln!(
                                    "  [cap] P{} → P{}: право [{}] передано в ответе (grant по IPC)",
                                    cur, dest,
                                    cap::rights_str(cap::rights(t.procs[dest].domain, nc).unwrap_or(Rights::NONE)),
                                );
                                cap::persist(); // передача права = чекпойнт c-space (Веха 21.3)
                            }
                        }
                        let df = &mut t.procs[dest].frame;
                        df.set_ret(n); // клиентский CALL вернёт число принятых байт
                        df.set_ret_at(1, tcap); // и дескриптор полученного права (MAX — не было)
                        // Веха 101 — и сколько байт сервер ХОТЕЛ отдать: иначе «ответ ровно такой»
                        // и «мой буфер оказался мал» с виду одно и то же (та же слепота, что была
                        // у запроса). Четвёртым значением, чтобы не трогать прежние два.
                        df.set_ret_at(3, len);
                        df.advance();
                        t.procs[dest].state = State::Runnable;
                        t.procs[dest].ready_at = arch::now_ticks();
                    }
                    let _ = cap::revoke(dom, Cap::from_bits(rcap as u64)); // одноразовость
                    0
                }
                Err(e) => {
                    vprintln!("  [ipc] P{} REPLY отклонён: {:?}  ← нет reply-capability", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance(); // сервер продолжает (остаётся current)
        }
        // SYS_CAP_DERIVE(cap, mask) -> новый дескриптор / MAX (Веха 21.1): урезанная копия
        // СВОЕГО права в СВОЁМ домене (права ∩ mask). GRANT не нужен — сужать то, чем владеешь,
        // безопасно всегда; передавать другим (CALL/REPLY с cap) — вот что требует GRANT.
        // Тоже чекпойнт: c-space меняется из userspace → фиксируем на диск.
        16 => {
            let (c, mask) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::derive(dom, Cap::from_bits(c as u64), Rights(mask as u32)) {
                Ok(nc) => {
                    vprintln!(
                        "  [cap] P{} CAP_DERIVE → копия с правами [{}] (аттенуация)",
                        cur,
                        cap::rights_str(cap::rights(dom, nc).unwrap_or(Rights::NONE)),
                    );
                    cap::persist();
                    nc.bits() as usize
                }
                Err(e) => {
                    vprintln!("  [cap] P{} CAP_DERIVE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_CAP_INFO(cap) -> (вид << 16 | права) | MAX (Веха 152.2): ОПИСАТЬ дескриптор.
        //
        // Read-only интроспекция для зонда конфайнмента ([[redteam]]): отличить «дотянулся до
        // store-read, которое и так есть» от «дотянулся до POWER, которого не давали». Это не
        // действие правом, а его описание — узнать вид можно только про cap, который уже держишь,
        // так что новой власти это не даёт, лишь называет уже достижимую. Гейта прав нет по той же
        // причине, что у прочей интроспекции (`SYS_TIME`, `SYS_CONSIZE`): вид cap-а не секрет.
        58 => {
            let c = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            // Веха 166 — в старших битах едет ещё и АДРЕСАТ эндпоинта (`aux`), а не только вид
            // с правами. Раскладка выбрана так, чтобы старые читатели ничего не заметили: они
            // берут `>> 16` в `u8` и младшие 16 бит, то есть выше 32-го бита не смотрят вовсе.
            let result = match cap::info_ex(dom, Cap::from_bits(c as u64)) {
                Ok((kind, rights, aux)) => {
                    (aux as usize) << 32 | (kind as usize) << 16 | rights.0 as usize
                }
                Err(_) => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        _ => return false,
    }
    true
}
