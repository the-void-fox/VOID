//! Веха 214.5 — **диспетчер системных вызовов VOID**: единственная дверь из процесса в ядро.
//!
//! Здесь разбор номера вызова и всё, что по нему делается: объекты и корни, права, каналы,
//! память, устройства, время, окно, ввод. Таблица процессов и её устройство — в `proc/mod.rs`,
//! планировщик — в `proc/sched.rs`, память — в `proc/space.rs`, чужая ABI — в `proc/lxabi.rs`.
//!
//! **Почему это отдельный файл (Веха 214.5).** Диспетчер — одна функция на две с половиной
//! тысячи строк; в такую нельзя заглянуть целиком, и соседство с ней делало нечитаемым всё
//! остальное в `proc.rs`. Резать его саму по группам вызовов — следующий шаг; сперва он
//! переехал как есть, чтобы переезд и разрезание не смешались в одной правке и не пришлось
//! искать, которая из двух что сломала.
//!
//! Подмодуль `proc`, а не сосед, по той же причине, что у соседей: работает приватными полями
//! `Table` и `Proc`.

use super::*;

/// Диспетчер syscall'ов. Номер в `a7`, аргументы в `a0..`, результат в `a0`. Работает прямо
/// с таблицей: IPC-вызовы затрагивают состояния/кадры ДРУГИХ процессов и выбор `current`.
pub(super) fn syscall(t: &mut Table, cur: usize) {
    let num = t.procs[cur].frame.syscall_num();
    // Веха 126.4 — хлебная крошка для аварийного дампа. Когда ядро прыгает по нулевому адресу,
    // кадр вызывающего уже затёрт, и по стеку не узнать даже, ЧЕЙ это был вызов. Две записи в
    // атомики на syscall стоят ничего, а отвечают на главный вопрос: кто именно.
    LAST_SYSCALL.store(num, Ordering::Relaxed);
    LAST_PROC.store(cur, Ordering::Relaxed);
    // Веха 213 — и та же крошка ПО ЯДРАМ. Две записи выше отвечают на «кто звал последним во
    // всей системе» (этого хватает аварийному дампу: авария одна). Зависание — другой вопрос:
    // там важно, чем занято КАЖДОЕ ядро, потому что тупик — это всегда двое.
    cpu::note_syscall(num);
    match num {
        // SYS_WRITE(ptr, len): напечатать буфер процесса (ядро читает U-память, SUM=1).
        1 => {
            let (ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            // Веха 23: буфер может лежать в ленивой куче — доотобразить до чтения ядром.
            let result = if ensure_heap_range(t, cur, ptr, len) {
                let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
                // Веха 214.1 — консоль защищает набранную строку ТОГО, КТО ЧИТАЕТ ввод: у
                // него одного на экране живёт приглашение с эхом набора. Признак берём у
                // ядра (`reads_console`, Веха 141.1), а не у пишущего: назвать себя шеллом
                // не должно быть возможно.
                let reads = t.procs[cur].reads_console;
                crate::print_user!(reads, "{}", core::str::from_utf8(bytes).unwrap_or("<?>"));
                len
            } else {
                usize::MAX
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_EXIT(code): завершить процесс, уступить следующему готовому. Если кто-то ждёт
        // этот процесс в SYS_EXEC (Веха 20.3) — разбудить, вернув ему код выхода.
        2 => {
            let code = t.procs[cur].frame.arg(0);
            // Веха 35: процесс уходит ЦЕЛИКОМ — все нити группы становятся Finished
            // (семантика exit()/возврата из main: прочие нити не переживают процесс).
            // Родитель ждал в SYS_EXEC ЛИДЕРА (его вернул SYS_EXEC) — будим по лидеру.
            let leader = t.procs[cur].group;
            vprintln!("  [proc] P{} SYS_EXIT({}) — процесс P{} (все нити группы)", cur, code, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                }
            }
            crate::net::ext_detach(leader); // Веха 195: карта ушла с процессом
            wake_exec_waiters(t, leader, code);
            if let Some(n) = t.next_runnable(cur) {
                t.set_cur(n);
            }
        }
        // SYS_YIELD: уступить следующему готовому.
        3 => {
            t.procs[cur].frame.advance();
            if let Some(n) = t.next_runnable(cur) {
                t.set_cur(n);
            }
        }
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
                    return;
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
                    return;
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
        // SYS_BLK_READ(dev_cap, sector, buf): шлюз к диску ПОД ЗАЩИТОЙ capability. Без валидного
        // cap на устройство (право READ) — отказ, даже если процесс знает номер сектора. DMA идёт
        // в ЯДЕРНЫЙ буфер (страницы процесса не identity-mapped), затем копируем вызывающему (SUM=1).
        7 => {
            let (dcap, sector, ubuf) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                // Веха 23: приёмный буфер может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, 512) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ сектор {} (по cap)", cur, sector);
                    let mut tmp = [0u8; 512];
                    let ok = crate::virtio_blk::read(sector as u64, &mut tmp);
                    if ok {
                        let dst = unsafe { core::slice::from_raw_parts_mut(ubuf as *mut u8, 512) };
                        dst.copy_from_slice(&tmp);
                    }
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ отклонён: {:?}  ← нет capability на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT(store_cap, buf, len, id_out) -> 0/MAX: сохранить значение в объектный
        // [[object-model|store]] (нужен cap на store с правом WRITE) и записать 32-байтный
        // content-id в id_out. Буферы читаются/пишутся в пространстве вызывающего (он current, SUM=1).
        8 => {
            let (scap, buf, len, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 22.2: буфер (и id_out — Веха 23) может лежать в ленивой куче —
                // доотобразить до того, как ядро его тронет.
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    // Веха 104 — нехватка памяти ядра здесь ОТКАЗ, а не паника: размер задаёт
                    // программа (а в пакетной фазе — сеть и чужой архив), и падать всей системой
                    // на чужой цифре недопустимо.
                    match crate::object::try_put(bytes) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!("  [obj] P{} OBJ_PUT {} байт → content-id (по cap)", cur, len);
                            0
                        }
                        None => {
                            println!("  [obj] P{} OBJ_PUT {} байт: НЕ ХВАТИЛО памяти ядра", cur, len);
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET(store_cap, id_ptr, out_buf, out_cap) -> длина (0 — нет; MAX — отказ):
        // прочитать значение по 32-байтному content-id (нужен cap на store с правом READ).
        //
        // Веха 114 — ВТОРЫМ значением возвращается НАСТОЯЩАЯ длина объекта. Без неё «объект ровно
        // с буфер» и «объект не влез» выглядели одинаково, и читатели росли удвоением буфера,
        // перечитывая объект по нескольку раз. Второе значение прежних читателей не задевает
        // (они берут только первое) — та же уловка, которой Веха 101 добавила «сколько хотели
        // отдать» к IPC.
        9 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let (result, whole) = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 22.2: приёмный буфер (и id_ptr — Веха 23) может лежать в ленивой куче —
                // доотобразить до записи ядром (весь ocap: лениво он выделился бы всё равно).
                Ok(()) if ensure_heap_range(t, cur, obuf, ocap)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    let (n, whole) = crate::object::with(&ContentId(id), |b| match b {
                        Some(bytes) => {
                            let m = bytes.len().min(ocap);
                            let out = unsafe { core::slice::from_raw_parts_mut(obuf as *mut u8, m) };
                            out.copy_from_slice(&bytes[..m]);
                            (m, bytes.len())
                        }
                        None => (0, 0),
                    });
                    vprintln!("  [obj] P{} OBJ_GET → {} байт из {} (по cap)", cur, n, whole);
                    (n, whole)
                }
                Ok(()) => (usize::MAX, 0), // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET отклонён: {:?}  ← нет capability на store", cur, e);
                    (usize::MAX, 0)
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.set_ret_at(1, whole);
            f.advance();
        }
        // SYS_OBJ_SET_ROOT(store_cap, name_ptr, name_len, id_ptr) -> 0/MAX: привязать именованный
        // корень к значению (нужен `WRITE`). Так объект переживает перезагрузку ([[persistent-store]]).
        10 => {
            let (scap, nptr, nlen, idp) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя и id могут лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            crate::object::set_root(name, ContentId(id));
                            // Веха 33: чекпойнт-на-каждый-чих сменился group commit —
                            // операция лишь копит счётчик, фиксацию делает политика
                            // ([`object::maybe_commit`] в resume(): порог или ~2 с).
                            vprintln!("  [obj] P{} OBJ_SET_ROOT '{}' (по cap, в пачку)", cur, name);
                            0
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_SET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET_ROOT(store_cap, name_ptr, name_len, id_out) -> 32 (есть) / 0 (нет) / MAX
        // (отказ): узнать content-id именованного корня (нужен `READ`).
        11 => {
            let (scap, nptr, nlen, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 23: имя и id_out могут лежать в ленивой куче — доотобразить.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => match crate::object::root(name) {
                            Some(id) => {
                                let out = unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                                out.copy_from_slice(&id.0);
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → есть (по cap)", cur, name);
                                32
                            }
                            None => {
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → нет (по cap)", cur, name);
                                0
                            }
                        },
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_BLK_WRITE(dev_cap, sector, buf, len) -> 0/MAX: записать сектор ПОД ЗАЩИТОЙ capability
        // (нужен `WRITE` на устройство). Данные копируем из буфера вызывающего (SUM=1) в ЯДЕРНЫЙ
        // буфер (страницы процесса не identity-mapped для DMA), недостающее до сектора — нулями.
        12 => {
            let (dcap, sector, ubuf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                // Веха 23: буфер данных может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, len.min(512)) => {
                    let mut tmp = [0u8; 512];
                    let n = len.min(512);
                    let src = unsafe { core::slice::from_raw_parts(ubuf as *const u8, n) };
                    tmp[..n].copy_from_slice(src);
                    let ok = crate::virtio_blk::write(sector as u64, &tmp);
                    vprintln!("  [blk] P{} SYS_BLK_WRITE сектор {} ({} байт, по cap)", cur, sector, n);
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_WRITE отклонён: {:?}  ← нет capability (WRITE) на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_DEL_ROOT(store_cap, name_ptr, name_len) -> 0 (снят) / 1 (не было) / MAX (отказ):
        // отвязать именованный корень (нужен `WRITE`). Объект уходит в GC, если больше ни на что не
        // сослан — это делает `unlink` в персоналии честным (Веха 18.3).
        13 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя может лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            // Веха 33: снятие корня тоже едет пачкой (group commit).
                            let existed = crate::object::del_root(name);
                            vprintln!("  [obj] P{} OBJ_DEL_ROOT '{}' → {} (по cap)", cur, name, if existed { "снят" } else { "не было" });
                            if existed { 0 } else { 1 }
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_DEL_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_MAP(len) -> VA | MAX (Веха 22.1): зарезервировать len байт кучи ЛЕНИВО — ни один
        // фрейм не выделяется сейчас; страницы придут по page fault ([`handle_user_fault`]) или
        // доотображением под шлюз ([`ensure_heap_range`]). Куча растёт вверх от
        // USER_HEAP_BASE_VA и не смеет дорасти до стека. Без capability: память — свой ресурс
        // процесса (квоты — отдельная история).
        17 => {
            // Веха 35: куча общая на группу — резервируем у лидера (замок таблицы
            // сериализует SYS_MAP разных нитей, гонки за heap_brk нет).
            let leader = t.procs[cur].group;
            let len = t.procs[cur].frame.arg(0);
            let start = t.procs[leader].heap_brk;
            let end = start.saturating_add(len.div_ceil(PAGE) * PAGE);
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            let result = if len == 0 || end > limit {
                usize::MAX
            } else {
                t.procs[leader].heap_brk = end;
                vprintln!(
                    "  [mm] P{} SYS_MAP {} байт → {:#x}..{:#x} (лениво, 0 фреймов)",
                    cur, len, start, end,
                );
                start
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
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
        // SYS_READ(buf, cap, nonblock) -> n: прочитать доступный ввод консоли (stdin) в буфер
        // процесса — хотя бы один байт. Ввода нет — процесс блокируется (StdinWait), sepc НЕ
        // двигаем: когда [`wait_stdin`] разбудит его по прерыванию UART, `ecall` РЕСТАРТУЕТ и на
        // этот раз заберёт байты из кольцевого буфера (Веха 20.2).
        //
        // Веха 99 — третий аргумент `nonblock`: вернуть 0 вместо сна. Нужен РЕАКТОРУ: хост чужих
        // процессов не может уснуть на клавиатуре, пока дети шлют ему вывод, — он обязан
        // обслуживать оба источника. Старые вызовы передают 0 и работают как прежде.
        14 => {
            let (buf, cap_len, nonblock) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 23: приёмный буфер может лежать в ленивой куче — доотобразить до записи ядром.
            if !ensure_heap_range(t, cur, buf, cap_len) {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            // Веха 141.1 — отметить процесс ЧИТАТЕЛЕМ консоли. Отметка нужна `wait_stdin`:
            // непрочитанные байты можно выбрасывать, только если их не читает НИКТО (оконный
            // сеанс), а узнать это ядру больше неоткуда. Ставится до сна: первое чтение обычно
            // и есть блокирующее.
            t.procs[cur].reads_console = true;
            let mut n = 0usize;
            while n < cap_len {
                let Some(b) = arch::console_getc() else { break };
                // Пишем в U-память вызывающего напрямую: он current, SUM=1 (как в SYS_WRITE).
                unsafe { *((buf + n) as *mut u8) = b };
                n += 1;
            }
            // Веха 101 — сказать вслух, если кольцо консоли переполнилось и ввод пропал. Место
            // выбрано здесь, а не в обработчике прерывания: печатать из него дорого и небезопасно,
            // а чтение — ровно тот момент, когда человек смотрит на результат набора.
            let lost = arch::console_take_lost();
            if lost > 0 {
                println!("  [tty] потеряно {} байт ввода — кольцо консоли переполнено", lost);
            }
            if n > 0 {
                let f = &mut t.procs[cur].frame;
                f.set_ret(n);
                f.advance();
            } else if nonblock != 0 {
                // Веха 99: ввода нет — честный ноль, без сна.
                let f = &mut t.procs[cur].frame;
                f.set_ret(0);
                f.advance();
            } else {
                // Блокировка до ввода с РЕСТАРТОМ: при пробуждении инструкция syscall'а
                // повторится (Веха 26: на riscv sepc и так на ecall, на x86 — откат rip).
                t.procs[cur].frame.restart();
                t.procs[cur].state = State::StdinWait;
                if let Some(nx) = t.next_runnable(cur) {
                    t.set_cur(nx);
                }
            }
        }
        // SYS_EXEC(store_cap, name_ptr, name_len, args_ptr, args_len) -> код выхода ребёнка /
        // MAX: запустить программу из store ПО ИМЕНИ КОРНЯ и ждать её завершения (foreground,
        // Веха 20.3). Требует права `EXEC` на store — ОТДЕЛЬНОГО от READ/WRITE: обладатель
        // может запускать программы, не умея читать или менять объекты (аттенуация «только
        // запуск»). Путь тот же, что в `exec_demo` ([[exec-from-store]]): корень → content-id →
        // байты ELF → [`elf::load`].
        //
        // Веха 30 — контракт запуска: `args` (NUL-разделённые записи, ≤ [`ARGS_MAX`]) станут
        // argv ребёнка после имени; env и таблица стартовых capability НАСЛЕДУЮТСЯ от
        // родителя (права — копиями через [`cap::endow`]: наделение потомка, не grant).
        // Веха 98 — `SYS_SPAWN` (41) — ТОТ ЖЕ путь запуска, но БЕЗ ожидания: родитель получает
        // номер ребёнка и продолжает работать. Одна ветка на оба вызова специально: расхождение
        // между «запустить» и «запустить и подождать» — источник тонких различий в наследовании
        // прав и окружения, а разница между ними ровно одна строка ниже.
        15 | 41 => {
            let wait_child = num == 15;
            let (scap, nptr, nlen, aptr, alen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4))
            };
            // Веха 98 — 6-й аргумент SPAWN: право, которое родитель ДОПОЛНИТЕЛЬНО отдаёт ребёнку
            // (обычно свой эндпоинт под stdio). `MAX` — нет такого.
            let extra_cap = if wait_child { usize::MAX } else { t.procs[cur].frame.arg(5) };
            // Веха 117 — 7-й аргумент: ИМЯ, под которым право объявится в окружении ребёнка.
            // 0 — прежнее `STDIO` (совместимость). Обобщение здесь уместно ровно потому, что
            // ядро и так не знает смысла этой строки: раз смысл userspace'а, то и имя тоже.
            // Композитору окон нужно своё (`WM`), иначе он был бы вынужден выдавать себя за
            // терминал.
            let key_ptr = if wait_child { 0 } else { t.procs[cur].frame.arg(6) };
            // Читаем имя СРАЗУ, пока мы заведомо в адресном пространстве РОДИТЕЛЯ: дальше по
            // ходу spawn'а ядро работает с пространством ребёнка (загрузка ELF), и та же строка
            // прочиталась бы уже не оттуда. Ошибка тихая — имя вышло бы мусором, а право
            // объявилось бы под ним же.
            let env_key: Option<Vec<u8>> =
                (key_ptr != 0).then(|| lx_cstr(t, cur, key_ptr, 16)).flatten();
            let dom = t.procs[cur].domain;
            let mut spawned = false;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::EXEC) {
                _ if alen > ARGS_MAX => {
                    vprintln!("  [exec] P{} SYS_EXEC: аргументы длиннее {} — отказ", cur, ARGS_MAX)
                }
                // Веха 23: имя (и аргументы) могут лежать в ленивой куче — доотобразить до чтения.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && (alen == 0 || ensure_heap_range(t, cur, aptr, alen)) =>
                {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        // Веха 26: `bin/<имя>` расширяется в арх-корень `bin/<arch>/<имя>` —
                        // процессы говорят «bin/hello», не зная архитектуры под собой.
                        // Веха 109 — АБСОЛЮТНЫЙ путь запускается из дерева пакета: так работает
                        // PATH профиля (`/nix/store/<путь>/bin/<имя>`). Всё прочее — по-прежнему
                        // корень store `bin/<arch>/<имя>`.
                        // Веха 153 — рядом с байтами берём content-id ОБРАЗА: диспетчер задач
                        // показывает хэш, не имя. У родного запуска из store он точный; у образа
                        // из дерева пакета (личность Linux) единого хэша нет — там `None`.
                        let (elf_bytes, image): (Option<Vec<u8>>, Option<ContentId>) =
                        if name.starts_with('/') {
                            (crate::lxfs::lookup(name.as_bytes())
                                .and_then(|m| crate::lxfs::read_all(&m)), None)
                        } else {
                            let full = crate::prog_root(name);
                            // Байты ELF копируем из store и сразу отпускаем его замок.
                            match crate::object::root(&full) {
                                Some(id) => (crate::object::with(&id, |b| b.map(Vec::from)), Some(id)),
                                None => (None, None),
                            }
                        };
                        match elf_bytes {
                            Some(bytes) => {
                                // Веха 89: памяти под новое пространство нет — отказ вызывающему
                                // (программа не запустилась), а не паника ядра.
                                let Some(root) = new_address_space() else {
                                    t.procs[cur].frame.set_ret(usize::MAX);
                                    return;
                                };
                                // argv ребёнка: имя + доп. аргументы вызывающего (общее для обоих путей).
                                let mut args = Vec::from(name.as_bytes());
                                args.push(0);
                                if alen > 0 {
                                    args.extend_from_slice(unsafe {
                                        core::slice::from_raw_parts(aptr as *const u8, alen)
                                    });
                                    if *args.last().unwrap() != 0 {
                                        args.push(0);
                                    }
                                }
                                // Имя процесса обязано жить дольше таблицы — утекает
                                // (запусков за сессию единицы, приемлемо до Вехи 22).
                                let pname: &'static str =
                                    Box::leak(String::from(name).into_boxed_str());
                                // Веха 38: тип ELF решает путь. Наш ET_EXEC — родной запуск
                                // (argv/env/старт-права через контракт); чужой static-PIE
                                // ET_DYN — linux-личность (стек Linux + трансля́тор syscall'ов).
                                let child = if elf::is_foreign(&bytes, USER_REGION_START) {
                                    let penv = t.procs[cur].env.clone();
                                    spawn_linux_locked(t, pname, &bytes, root, args, &penv)
                                } else {
                                    match elf::load(root, &bytes, USER_HEAP_BASE_VA) {
                                        Ok(entry) => {
                                            let c = create_process_locked(t, pname, root, entry, 0);
                                            let parent_env = t.procs[cur].env.clone();
                                            t.procs[c].args = args;
                                            t.procs[c].env = parent_env;
                                            Some(c)
                                        }
                                        Err(e) => {
                                            vprintln!(
                                                "  [exec] P{} SYS_EXEC '{}': негодный ELF: {:?}",
                                                cur, name, e,
                                            );
                                            None
                                        }
                                    }
                                };
                                if let Some(child) = child {
                                    // Веха 153 — что именно исполняется (хэш образа); «системным»
                                    // ребёнок SYS_EXEC/SPAWN не становится (истоком системного
                                    // может быть только init из конфига поколения).
                                    t.procs[child].image = image;
                                    // Стартовые capability наследуются копиями (`cap::endow`) —
                                    // и родному ребёнку, и linux-процессу (тому — на будущее,
                                    // под файловую персоналию; stdio он шлёт напрямую в консоль).
                                    let cdom = t.procs[child].domain;
                                    for bits in t.procs[cur].start_caps.clone() {
                                        let pc = Cap::from_bits(bits as u64);
                                        // Веха 154 — право с пометкой «не наследуется»
                                        // (`mmio:fb!`/`power!`) остаётся у родителя: композитор
                                        // держит экран и выключение при себе, а не раздаёт их
                                        // каждому окну, которое открывает.
                                        if !cap::inheritable(dom, pc) {
                                            continue;
                                        }
                                        if let Ok(c) = cap::endow(dom, pc, cdom) {
                                            t.procs[child].start_caps.push(c.bits() as usize);
                                        }
                                    }
                                    vprintln!(
                                        "  [exec] P{} SYS_EXEC '{}' → P{} ({}; ждёт завершения; env {} Б, старт-прав {})",
                                        cur, name, child,
                                        if t.procs[child].linux { "linux-abi" } else { "native" },
                                        t.procs[child].env.len(), t.procs[child].start_caps.len(),
                                    );
                                    // Веха 98 — наделить ребёнка ДОПОЛНИТЕЛЬНЫМ правом и назвать
                                    // его в окружении. Индекс кладёт ЯДРО, потому что только оно
                                    // знает, сколько прав ребёнок унаследовал; выдумывать его на
                                    // стороне родителя значило бы дублировать эту арифметику и
                                    // разъезжаться с ней при первом же изменении.
                                    //
                                    // Ядро при этом НЕ узнаёт, что такое stdio: оно кладёт
                                    // строку в окружение — ровно как уже кладёт имя программы в
                                    // argv ([[process-contract]]). Смысл строки — дело userspace.
                                    if extra_cap != usize::MAX {
                                        if let Ok(c) =
                                            cap::endow(dom, Cap::from_bits(extra_cap as u64), cdom)
                                        {
                                            let idx = t.procs[child].start_caps.len();
                                            t.procs[child].start_caps.push(c.bits() as usize);
                                            let key = env_key
                                                .as_deref()
                                                .and_then(|k| core::str::from_utf8(k).ok())
                                                .filter(|k| !k.is_empty())
                                                .unwrap_or("STDIO");
                                            let mut line = alloc::format!("{}={}\0", key, idx);
                                            let env = &mut t.procs[child].env;
                                            // Веха 147 — СНАЧАЛА выкидываем прежнюю запись с тем
                                            // же ключом, унаследованную от родителя.
                                            //
                                            // Окружение наследуется целиком, и без этой уборки в
                                            // блобе оказывались ДВА `STDIO=`: свой и родительский.
                                            // Читатель берёт первый попавшийся — то есть чужой, и
                                            // ребёнок начинал говорить не с тем хозяином. Ловилось
                                            // это так: терминал, запущенный из-под сторожа
                                            // (Веха 147), отдавал свой шелл СТОРОЖУ, тот отвечал
                                            // «ввода нет», и шелл умирал на первом же чтении.
                                            //
                                            // `KEY=VAL` — словарь, и двух значений у ключа быть не
                                            // может; чинить это на стороне читателей значило бы
                                            // чинить в трёх местах вместо одного.
                                            let pref = alloc::format!("{}=", key);
                                            let mut clean: Vec<u8> = Vec::with_capacity(env.len());
                                            for rec in env.split(|&b| b == 0) {
                                                if rec.is_empty() || rec.starts_with(pref.as_bytes())
                                                {
                                                    continue;
                                                }
                                                clean.extend_from_slice(rec);
                                                clean.push(0);
                                            }
                                            *env = clean;
                                            // Окружение — блоб `KEY=VAL\0…`; хвостовой NUL уже есть.
                                            unsafe { env.append(line.as_mut_vec()) };
                                        }
                                    }
                                    // Веха 152.3 — потолок наделения: ребёнок унаследовал
                                    // start_caps родителя (и, может, доп-право STDIO); теперь
                                    // можно отозвать персистентные права его домена (тёзка прошлой
                                    // загрузки), не покрытые этим наделением. Так `run probe`
                                    // урезанным поколением не дотянется до store:rwg, оставленного
                                    // привилегированным тёзкой в прошлой жизни (находка №1).
                                    let cendow: Vec<Cap> = t.procs[child]
                                        .start_caps
                                        .iter()
                                        .map(|&b| Cap::from_bits(b as u64))
                                        .collect();
                                    cap::clamp_persisted(t.procs[child].domain, &cendow);
                                    // Родство записывается в ОБОИХ случаях (Веха 114). Раньше его
                                    // ставил только SPAWN, потому что нужно оно было лишь для
                                    // `SYS_WAIT`/`SYS_KILL`; из-за этого дерево процессов
                                    // обрывалось на каждом `run`, и мультиплексор не мог понять,
                                    // чей вывод к нему пришёл (см. `SYS_PARENT`).
                                    t.procs[child].parent = cur;
                                    if wait_child {
                                        // Родитель ждёт ребёнка; sepc/a0 выставит wake_exec_waiters.
                                        t.procs[cur].state = State::ExecWait(child);
                                        t.set_cur(child);
                                    } else {
                                        // Веха 98 — SPAWN: родителю сразу отдаём номер ребёнка и
                                        // ПРОДОЛЖАЕМ его. Ребёнок помечается зомби — его слот не
                                        // переиспользуется, пока родитель не заберёт код выхода.
                                        t.procs[child].zombie = true;
                                        let f = &mut t.procs[cur].frame;
                                        f.set_ret(child);
                                        f.advance();
                                    }
                                    spawned = true;
                                }
                            }
                            None => vprintln!("  [exec] P{} SYS_EXEC: корня '{}' нет в store", cur, name),
                        }
                    }
                }
                Ok(()) => vprintln!("  [exec] P{} SYS_EXEC: фреймы кончились под ленивый буфер имени", cur),
                Err(e) => vprintln!(
                    "  [exec] P{} SYS_EXEC отклонён: {:?}  ← нет capability (EXEC) на store",
                    cur, e,
                ),
            }
            if !spawned {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            }
        }
        // SYS_ARGS(sel, buf, len) -> полная длина блоба (в buf скопировано min(len, полная)):
        // sel 0 — argv (NUL-разделённые записи, [0] — имя программы), 1 — env (`KEY=VAL\0…`).
        // Контракт запуска Вехи 30: то, что Linux кладёт на стек при execve, у нас процесс
        // спрашивает у ядра — раскладка стека остаётся целиком делом программы.
        18 => {
            let (sel, ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 35: argv/env — свойство процесса, живут у лидера группы нитей.
            let leader = t.procs[cur].group;
            let blob = match sel {
                0 => Some(t.procs[leader].args.clone()),
                1 => Some(t.procs[leader].env.clone()),
                _ => None,
            };
            let result = match blob {
                None => usize::MAX,
                Some(b) => {
                    let n = len.min(b.len());
                    if n == 0 || ensure_heap_range(t, cur, ptr, n) {
                        if n > 0 {
                            unsafe {
                                core::ptr::copy_nonoverlapping(b.as_ptr(), ptr as *mut u8, n)
                            };
                        }
                        b.len()
                    } else {
                        usize::MAX
                    }
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_STARTCAP(i) -> биты i-го стартового capability | MAX (конец таблицы).
        // Преоткрытые права процесса (как preopen'ы WASI): выданы ядром при spawn'е или
        // унаследованы от родителя при SYS_EXEC. Дескрипторы валидны в СВОЁМ домене.
        19 => {
            let i = t.procs[cur].frame.arg(0);
            // Веха 35: стартовые capability — у лидера группы (нить делит домен процесса).
            let leader = t.procs[cur].group;
            let bits = t.procs[leader].start_caps.get(i).copied().unwrap_or(usize::MAX);
            let f = &mut t.procs[cur].frame;
            f.set_ret(bits);
            f.advance();
        }
        // SYS_NET_SEND(dev_cap, buf, len) -> 0/MAX (Веха 34): отправить сырой Ethernet-кадр.
        // Нужен cap на сетевое устройство (право WRITE). Кадр читается из U-памяти (SUM=1),
        // копируется в ядерный TX-буфер драйвера (страницы процесса не identity-mapped).
        20 => {
            let (dcap, buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            // Веха 195 — кого разбудил этот кадр (карта в процессе); ход ему отдаём ниже.
            let mut netdev_woken = None;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                Ok(cap::Device::Net) if len <= 2048 && ensure_heap_range(t, cur, buf, len) => {
                    let mut tmp = [0u8; 2048];
                    let src = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    tmp[..len].copy_from_slice(src);
                    vprintln!("  [net] P{} SYS_NET_SEND {} байт (по cap)", cur, len);
                    let ok = crate::net::send(&tmp[..len]);
                    // Веха 195 — если карта живёт в процессе, `send` только положил кадр в
                    // очередь. Разбудить драйвера обязан тот, у кого в руках таблица процессов:
                    // иначе он узнает про кадр на своём следующем таймере, то есть через
                    // десятки миллисекунд, а опрос очереди вхолостую жёг бы процессор в простое.
                    netdev_woken = wake_netdev_owner(t);

                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX,
                Err(e) => {
                    vprintln!("  [net] P{} SYS_NET_SEND отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
            // Веха 195 — ОТДАТЬ ХОД ДРАЙВЕРУ, а не просто сделать его готовым.
            //
            // Разбудить оказалось мало, и это видно на замере: кадр лежал в очереди 16–20 мс,
            // то есть ровно квант вытеснения. Причина не в планировщике, а в том, кто зовёт:
            // `net-srv`, отправив кадр, не блокируется — он идёт качать стек дальше, и ядро
            // законно продолжает его до конца кванта. Драйвер в это время готов и ждёт хода.
            //
            // Будь карта в ядре, отправка кончилась бы записью в кольцо и звонком в дверь, то
            // есть НА МЕСТЕ. Уступка хода — это то же самое, только когда «кольцо» находится в
            // другом процессе: остаток кванта дарится тому, кто доведёт кадр до провода.
            if let Some(pid) = netdev_woken {
                if t.procs[pid].state == State::Runnable && t.proc_free(pid, cpu::id()) {
                    t.set_cur(pid);
                }
            }
        }
        // SYS_NET_RECV(dev_cap, buf, buflen) -> длина кадра (0 — пусто; MAX — отказ).
        // Неблокирующий опрос приёмного кольца (нужен cap на устройство, право READ).
        21 => {
            let (dcap, buf, buflen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                Ok(cap::Device::Net) if ensure_heap_range(t, cur, buf, buflen.min(2048)) => {
                    let mut tmp = [0u8; 2048];
                    let cap_len = buflen.min(2048);
                    let n = crate::net::recv(&mut tmp[..cap_len]);
                    if n > 0 {
                        let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n) };
                        dst.copy_from_slice(&tmp[..n]);
                        vprintln!("  [net] P{} SYS_NET_RECV {} байт (по cap)", cur, n);
                    }
                    n
                }
                Ok(_) => usize::MAX,
                Err(_) => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_NET_MAC(dev_cap, buf6) -> 0/MAX (Веха 34): записать MAC карты (6 байт).
        22 => {
            let (dcap, buf) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                // Веха 132.2 — НЕТ КАРТЫ значит отказ, а не нулевой MAC. Прежде вызов «удавался»
                // с адресом 00:00:00:00:00:00, и `net-srv` поднимал стек над пустотой: в одном
                // логе стояло и «сетевой карты нет», и «net-srv запущен, MAC 00:…» с попыткой
                // DHCP. Система противоречила сама себе, и это заметил владелец.
                Ok(cap::Device::Net) if crate::net::present() && ensure_heap_range(t, cur, buf, 6) => {
                    let mac = crate::net::mac();
                    let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, 6) };
                    dst.copy_from_slice(&mac);
                    0
                }
                Ok(_) => usize::MAX,
                Err(_) => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_NETDEV(netdrv_cap, op, buf, len) — **БЫТЬ сетевой картой** (Веха 195).
        //
        // Это вторая сторона `SYS_NET_SEND`/`SYS_NET_RECV`: там процесс пользуется картой, здесь
        // процесс ею ЯВЛЯЕТСЯ. Нужно затем, что настоящие драйверы у нас хостируемые (неизменённый
        // код Linux в процессе), и до этой вехи принятые кадры до стека не доходили вовсе —
        // `netif_receive_skb` в шиме их освобождал. Теперь тот же кадр едет в ядро, а `net-srv`
        // достаёт его обычным `net_recv` и не знает, что карта сменила сторону.
        //
        // Право отдельного вида (`Device::NetDrv`, минтит только `init`): говорить от имени
        // провода — не то же, что ходить в сеть, и смешивать это значило бы разрешить первое
        // каждому, кому разрешили второе.
        //
        //   op 0 — представиться: `buf` = 6 байт MAC. С этого мгновения карта системы — этот
        //          процесс;
        //   op 1 — принятый кадр в ядро (`buf`, `len`); 0 — взят, MAX — очередь полна;
        //   op 2 — забрать исходящий кадр (`buf`, `len` = размер буфера) → длина, 0 — пусто;
        //   op 3 — отсоединиться (умирающий драйвер; смерть процесса делает это и сама).
        64 => {
            let (dcap, op, buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let leader = t.procs[cur].group;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                Ok(cap::Device::NetDrv) => match op {
                    0 if ensure_heap_range(t, cur, buf, 6) => {
                        let mut mac = [0u8; 6];
                        let src = unsafe { core::slice::from_raw_parts(buf as *const u8, 6) };
                        mac.copy_from_slice(src);
                        crate::net::ext_attach(leader, mac);
                        0
                    }
                    1 if len <= 2048 && ensure_heap_range(t, cur, buf, len) => {
                        let mut tmp = [0u8; 2048];
                        let src = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                        tmp[..len].copy_from_slice(src);
                        let ok = crate::net::ext_rx_push(&tmp[..len]);

                        // Разбудить СТЕК тем же признаком, каким его будит прерывание настоящей
                        // карты (Веха 91). Без этой строки кадр лежал в очереди до следующего
                        // холостого круга `net-srv`, и цена была видна на замере: `ping` через
                        // хостируемый драйвер отвечал за 40 000 мкс против 40 мкс через
                        // ядерный e1000 — в тысячу раз медленнее, причём не из-за драйвера.
                        // Разливает признак `resume`, то есть пробуждение случится сразу по
                        // возврате из этого самого вызова.
                        if ok {
                            on_net_irq();
                        }
                        if ok { 0 } else { usize::MAX }
                    }
                    2 if ensure_heap_range(t, cur, buf, len.min(2048)) => {
                        let mut tmp = [0u8; 2048];
                        let cap_len = len.min(2048);
                        let n = crate::net::ext_tx_pop(&mut tmp[..cap_len]);

                        if n > 0 {
                            let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n) };
                            dst.copy_from_slice(&tmp[..n]);
                        }
                        n
                    }
                    3 => {
                        crate::net::ext_detach(leader);
                        0
                    }
                    _ => usize::MAX,
                },
                Ok(_) => usize::MAX,
                Err(e) => {
                    vprintln!("  [net] P{} SYS_NETDEV отклонён: {:?}  ← нет права быть картой", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_THREAD_SPAWN(entry, arg, stack_top) -> tid | MAX (Веха 35): завести НИТЬ в
        // текущем процессе — контекст в ТОМ ЖЕ адресном пространстве и домене, со своим
        // стеком (`stack_top` — вершина, userspace выделяет его из кучи процесса лениво).
        // Возвращает id нити (для THREAD_JOIN). Прав не требует: нить — та же единица
        // защиты, что процесс (не расширяет полномочий).
        23 => {
            let (entry, arg, stack_top) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let leader = t.procs[cur].group;
            let tid = create_thread_locked(t, leader, entry, arg, stack_top);
            vprintln!(
                "  [thread] P{} SYS_THREAD_SPAWN → нить P{} (вход {:#x}, стек {:#x})",
                cur, tid, entry, stack_top,
            );
            let f = &mut t.procs[cur].frame;
            f.set_ret(tid);
            f.advance();
        }
        // SYS_THREAD_EXIT(retval) (Веха 35): завершить ТЕКУЩУЮ нить (не процесс), отдать
        // `retval` присоединяющимся (THREAD_JOIN). Не возвращается в вызывающего. Возврат
        // из main или std::process::exit идут через SYS_EXIT — тот кладёт всю группу.
        24 => {
            let retval = t.procs[cur].frame.arg(0);
            vprintln!("  [thread] P{} SYS_THREAD_EXIT({})", cur, retval);
            t.procs[cur].state = State::Finished;
            t.procs[cur].retval = retval;
            wake_join_waiters(t, cur, retval);
            if let Some(n) = t.next_runnable(cur) {
                t.set_cur(n);
            }
        }
        // SYS_THREAD_JOIN(tid) -> retval | MAX (Веха 35): дождаться завершения нити `tid`
        // своей группы и забрать её `retval`. MAX — нет такой нити / чужая группа / это мы
        // сами. Уже завершилась — вернуть сразу; иначе блок (JoinWait), пробуждение выставит
        // a0/advance (как ExecWait: не рестарт).
        25 => {
            let tid = t.procs[cur].frame.arg(0);
            let joinable =
                tid < t.procs.len() && tid != cur && t.procs[tid].group == t.procs[cur].group;
            if !joinable {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            } else if t.procs[tid].state == State::Finished {
                let rv = t.procs[tid].retval;
                let f = &mut t.procs[cur].frame;
                f.set_ret(rv);
                f.advance();
            } else {
                t.procs[cur].state = State::JoinWait(tid);
                if let Some(n) = t.next_runnable(cur) {
                    t.set_cur(n);
                }
            }
        }
        // SYS_FUTEX(op, uaddr, val, timeout) (Веха 35): примитив блокировки для Mutex/Condvar/
        // Parker в std. op 0 — WAIT(uaddr, expected, timeout_ticks): уснуть, если *uaddr ещё
        // == expected (иначе сразу 0 — «значение сменилось»); timeout в тиках [`arch::now_ticks`]
        // (0 — бессрочно). op 1 — WAKE(uaddr, count): разбудить до count спящих на слове,
        // вернуть число. Ключ ожидания — (адресное пространство, uaddr): futex-слова процесса
        // общие для его нитей. WAIT возвращает 0 (разбужен) либо 1 (истёк таймаут).
        26 => {
            let (op, uaddr, val, timeout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            match op {
                0 => {
                    // futex-слово обычно в куче (Arc/Box) — доотобразить до чтения ядром.
                    let read = if ensure_heap_range(t, cur, uaddr, 4) {
                        Some(unsafe { core::ptr::read_volatile(uaddr as *const u32) })
                    } else {
                        None
                    };
                    match read {
                        Some(v) if v == val as u32 => {
                            let deadline = if timeout == 0 {
                                None
                            } else {
                                Some(arch::now_ticks().wrapping_add(timeout as u64))
                            };
                            t.procs[cur].state = State::FutexWait;
                            t.procs[cur].futex_addr = uaddr;
                            t.procs[cur].futex_deadline = deadline;
                            if let Some(n) = t.next_runnable(cur) {
                                t.set_cur(n);
                            }
                        }
                        _ => {
                            // Значение уже иное (или недоступно) — не спать (EAGAIN): 0.
                            let f = &mut t.procs[cur].frame;
                            f.set_ret(0);
                            f.advance();
                        }
                    }
                }
                _ => {
                    let space = t.procs[cur].space;
                    let woken = wake_futex(t, space, uaddr, val);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(woken);
                    f.advance();
                }
            }
        }
        // SYS_SET_TLS(ptr) (Веха 35): задать TLS-указатель нити (tp на riscv / база %fs на
        // x86). Userspace строит per-thread TLS-блок и сообщает его базу; ядро восстанавливает
        // указатель на каждом входе в U ([`arch::TrapFrame::set_thread_ptr`]).
        27 => {
            let tp = t.procs[cur].frame.arg(0);
            t.procs[cur].frame.set_thread_ptr(tp);
            vprintln!("  [thread] P{} SYS_SET_TLS {:#x}", cur, tp);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_CHECKPOINT(scap, name, len) — Веха 37: заморозить СЕБЯ в store (право WRITE
        // на store: чекпойнт ПИШЕТ объекты). Образ — под корнем `proc/<arch>/<имя>`.
        // Семантика setjmp: живому возвращается 0 (образ снят, работает дальше),
        // РАЗМОРОЖЕННОМУ из образа — 1 («возврат из прошлой жизни»); MAX — отказ.
        // Морозится только лидер группы без других живых нитей (кадр один).
        28 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let leader = t.procs[cur].group;
            let solo = cur == leader
                && (0..t.procs.len()).all(|i| {
                    i == cur || t.procs[i].group != leader || t.procs[i].state == State::Finished
                });
            let mut ret = usize::MAX;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if solo && nlen > 0 && nlen <= 64 && ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        // Кадр образа: результат 1 и продвинутый pc — размороженный
                        // очнётся РОВНО в возврате из этого syscall'а.
                        let mut ff = t.procs[cur].frame;
                        ff.set_ret(1);
                        ff.advance();
                        let root_name = alloc::format!("proc/{}/{}", arch::ARCH_NAME, name);
                        let (space, brk) = (t.procs[cur].space, t.procs[cur].heap_brk);
                        let (args, env) = (t.procs[cur].args.clone(), t.procs[cur].env.clone());
                        let pages = crate::checkpoint::freeze(
                            &root_name, space, &ff, brk, &args, &env,
                            USER_REGION_START, USER_STACK_TOP_VA,
                        );
                        // Чекпойнт обязан быть НА ДИСКЕ к возврату syscall'а — иначе
                        // «образ» жил бы в RAM до ближайшего простоя (Веха 33).
                        crate::object::commit_if_dirty();
                        vprintln!(
                            "  [ckpt] P{} SYS_CHECKPOINT '{}' — {} страниц, коммит (по cap)",
                            cur, root_name, pages,
                        );
                        ret = 0;
                    }
                }
                Ok(()) => vprintln!(
                    "  [ckpt] P{} SYS_CHECKPOINT: отказ (другие нити живы / не лидер / имя негодно)",
                    cur,
                ),
                Err(e) => vprintln!(
                    "  [ckpt] P{} SYS_CHECKPOINT отклонён: {:?}  ← нет capability (WRITE) на store",
                    cur, e,
                ),
            }
            let f = &mut t.procs[cur].frame;
            f.set_ret(ret);
            f.advance();
        }
        // SYS_RESTORE(scap, name, len) — Веха 37: разморозить процесс из образа
        // `proc/<arch>/<имя>` (право EXEC — это запуск процесса, как SYS_EXEC, и ждём
        // так же). args/env приезжают ИЗ ОБРАЗА (программа их уже прочла), стартовые
        // capability — свежее наследство размораживающего (права не консервируются:
        // дескрипторы прошлой жизни умерли вместе с ней — модель exec, не пленение).
        29 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let mut spawned = false;
            match cap::store(dom, Cap::from_bits(scap as u64), Rights::EXEC) {
                Ok(()) if nlen > 0 && nlen <= 64 && ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        let root_name = alloc::format!("proc/{}/{}", arch::ARCH_NAME, name);
                        match crate::checkpoint::thaw(&root_name) {
                            Some(img) => {
                                let parent_scaps = t.procs[cur].start_caps.clone();
                                let pname: &'static str = Box::leak(
                                    alloc::format!("thaw:{}", name).into_boxed_str(),
                                );
                                let child = create_process_locked(t, pname, img.root, 0, 0);
                                let cdom = t.procs[child].domain;
                                t.procs[child].frame = img.frame;
                                t.procs[child].heap_brk = img.heap_brk;
                                t.procs[child].args = img.args;
                                t.procs[child].env = img.env;
                                for bits in parent_scaps {
                                    if let Ok(c) =
                                        cap::endow(dom, Cap::from_bits(bits as u64), cdom)
                                    {
                                        t.procs[child].start_caps.push(c.bits() as usize);
                                    }
                                }
                                // Веха 152.3 — потолок наделения и для размороженного: власть —
                                // наследство размораживающего, персистентное сверх неё отозвать.
                                let cendow: Vec<Cap> = t.procs[child]
                                    .start_caps
                                    .iter()
                                    .map(|&b| Cap::from_bits(b as u64))
                                    .collect();
                                cap::clamp_persisted(cdom, &cendow);
                                vprintln!(
                                    "  [ckpt] P{} SYS_RESTORE '{}' → P{} ({} страниц; ждёт завершения, права — наследство размораживающего)",
                                    cur, root_name, child, img.pages,
                                );
                                t.procs[cur].state = State::ExecWait(child);
                                t.set_cur(child);
                                spawned = true;
                            }
                            None => vprintln!(
                                "  [ckpt] P{} SYS_RESTORE: образа '{}' нет, он чужой архитектуры или негоден",
                                cur, root_name,
                            ),
                        }
                    }
                }
                Ok(()) => vprintln!("  [ckpt] P{} SYS_RESTORE: имя негодно", cur),
                Err(e) => vprintln!(
                    "  [ckpt] P{} SYS_RESTORE отклонён: {:?}  ← нет capability (EXEC) на store",
                    cur, e,
                ),
            }
            if !spawned {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
            }
        }
        // SYS_INSTALL(store_cap, op, slot, buf, len) (Вехи 48, 174): установить VOID на SATA-диск
        // из загрузочного модуля multiboot2 (образ с носителя) — либо СПРОСИТЬ, какие диски есть.
        //
        //   op = 0 — перечислить диски: в `buf` пишутся записи по [`INSTALL_REC`] байт, возврат —
        //            сколько записано. Столько же, сколько влезло в `len`.
        //   op = 1 — установить на диск с номером `slot`; возврат — сектор начала store | MAX.
        //
        // Оба под одним правом — store-cap с WRITE (у shell'а `store:xw`): установка меняет
        // содержимое store целиком, право по силе равно записи. Список дисков сам по себе
        // безобиден, но отдельного права под него мы не заводим: спрашивает его ровно тот, кто
        // собирается ставить, а лишний вид права — лишняя вещь, которую надо объяснять.
        //
        // ДИСК СТИРАЕТСЯ ЦЕЛИКОМ. Ставить на диск, с которого работает система, ядро отказывает.
        30 => {
            let (scap, op, slot, ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Err(e) => {
                    vprintln!("  [install] P{} отклонён: {:?}  ← нет capability (WRITE) на store", cur, e);
                    usize::MAX
                }
                Ok(()) if op == 0 => {
                    let want = (len / INSTALL_REC).min(crate::arch::MAX_DISKS);
                    let mut disks = [crate::ahci::Disk {
                        slot: 0,
                        sectors: 0,
                        model: [0; crate::ahci::MODEL_LEN],
                        void: false,
                        live: false,
                    }; crate::arch::MAX_DISKS];
                    let n = crate::ahci::disks(&mut disks[..want]);
                    // Веха 194.1: NVMe-диски идут ПОСЛЕ портов AHCI — своими номерами (сотня),
                    // чтобы выбор человека значил одно и то же и на машине с двумя шинами.
                    let n = n + crate::nvme::disks(&mut disks[n..want]);
                    // Веха 196 — и USB-накопители, своей сотней (200+). Порядок тот же:
                    // сперва внутренние шины, потом съёмное.
                    let n = n + crate::xhci::disks(&mut disks[n..want]);
                    if !ensure_heap_range(t, cur, ptr, n * INSTALL_REC) {
                        usize::MAX
                    } else {
                        for (i, d) in disks.iter().take(n).enumerate() {
                            // SAFETY: диапазон проверен `ensure_heap_range` — он же дотянул
                            // ленивые страницы кучи, в которые пишем.
                            let rec = unsafe {
                                core::slice::from_raw_parts_mut(
                                    (ptr + i * INSTALL_REC) as *mut u8,
                                    INSTALL_REC,
                                )
                            };
                            rec.fill(0);
                            rec[0..8].copy_from_slice(&d.sectors.to_le_bytes());
                            rec[8..12].copy_from_slice(&(d.slot as u32).to_le_bytes());
                            rec[12] = (d.void as u8) | (d.live as u8) << 1;
                            rec[16..16 + crate::ahci::MODEL_LEN].copy_from_slice(&d.model);
                        }
                        n
                    }
                }
                Ok(()) => match crate::install::run(slot) {
                    Ok(p2) => {
                        crate::println!("  [install] VOID установлен на диск {} (store с сектора {}) — перезагрузись без носителя", slot, p2);
                        p2 as usize
                    }
                    Err(e) => {
                        crate::println!("  [install] отказ: {}", e);
                        usize::MAX
                    }
                },
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_MMIO_MAP(mmio_cap, va) -> 0 | MAX (Веха 51): замапить окно MMIO устройства (из cap
        // база+длина) в адресное пространство userspace-драйвера по адресу `va`. Так драйвер в
        // userspace получает регистры железа — без cap доступа нет. `va` — в USER-регионе, вне
        // стека (драйвер сам выбирает окно). Пер-страничное отображение U|R|W.
        31 => {
            let (mcap, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::mmio(dom, Cap::from_bits(mcap as u64), Rights::WRITE) {
                Ok((base, len)) => {
                    let pages = len.div_ceil(PAGE);
                    let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
                    // Веха 117 — ЭКРАН отображается write-combining, регистры устройств — нет.
                    // Разница принципиальная: фреймбуферу нужна полоса (записи копятся и уходят
                    // пачками), а регистру нужен ПОРЯДОК — слитая или переставленная запись в
                    // него ломает устройство. Поэтому WC получает ровно одно окно — то, которое
                    // арх признал экраном.
                    let is_screen = arch::video_window() == Some((base, len));
                    let attr = if is_screen { arch::MAP_WC } else { 0 };
                    if va >= USER_REGION_START && va + pages * PAGE <= limit && base % PAGE == 0 {
                        let root = arch::space_root(t.procs[cur].space);
                        let mut ok = true;
                        for i in 0..pages {
                            ok &= unsafe {
                                arch::map(root, va + i * PAGE, base + i * PAGE,
                                    arch::MAP_R | arch::MAP_W | arch::MAP_U | attr)
                            };
                            if !ok {
                                break; // Веха 89: нет памяти под таблицы — отказ драйверу
                            }
                        }
                        // Веха 170 — адрес назвал драйвер, отображение могло лечь поверх
                        // прежнего: сброс нужен всем, кто стоит на этом пространстве.
                        flush_space(t, t.procs[cur].space);
                        if !ok {
                            usize::MAX
                        } else {
                            // Веха 97: замаплен ЭКРАН — ядро уступает его и уходит в serial.
                            // Единственная точка передачи владения: раньше отдавать нечего
                            // (окно не отображено), позже — некому.
                            if is_screen {
                                arch::video_give_to_user(cur);
                                println!(
                                    "  [видео] экран отдан процессу P{} (WC) — вывод ядра уходит в serial",
                                    cur
                                );
                            }
                            vprintln!("  [drv] P{} SYS_MMIO_MAP {:#x} ({} стр.) → {:#x}", cur, base, pages, va);
                            0
                        }
                    } else {
                        usize::MAX
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_MMIO_MAP отклонён: {:?}  ← нет cap на MMIO", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_DMA_ALLOC(dma_cap, va, pages) -> физ-адрес | MAX (Веха 51; страницы — Веха 133):
        // выделить `pages` ПОДРЯД идущих обнулённых фреймов, замапить их в драйвер начиная с `va`
        // (U|R|W) и вернуть ФИЗИЧЕСКИЙ адрес начала — им драйвер программирует DMA устройства.
        // Без IOMMU это доверенное право (dma-cap только у драйверов).
        //
        // Непрерывность обязательна и не сводится к нескольким вызовам по странице: кольцо
        // дескрипторов карта обходит сама, о таблицах страниц не зная. `pages == 0` читаем как 1 —
        // так продолжают работать драйверы, написанные до этой вехи.
        32 => {
            let (dcap, va, pages) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2).max(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::dma(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                Ok(()) => {
                    let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
                    if va >= USER_REGION_START && va + pages * PAGE <= limit {
                        match frame::alloc_contig(pages) {
                            Some(pa) => {
                                let root = arch::space_root(t.procs[cur].space);
                                let mut ok = true;
                                // Веха 213.2 — `MAP_SHARED`: умирающий процесс эти страницы НЕ
                                // освобождает. Пометка та же, что у разделяемой памяти, и смысл
                                // у неё тот же — «не твои, не возвращай», — но причина здесь
                                // сильнее.
                                //
                                // В эти страницы пишет ЖЕЛЕЗО, по физическим адресам, и оно не
                                // знает, что процесс умер. Ядро не может ни спросить устройство,
                                // остановилось ли оно, ни остановить его само: драйвер живёт в
                                // userspace, и регистры карты знает только он. Значит вернуть
                                // такую страницу в общий котёл — это отдать чужой памяти живого
                                // писателя.
                                //
                                // Так и вышло: Wi-Fi-разведка выходила, оставив карте кольцо
                                // приёма, карта продолжала класть маяки по тем же адресам, и
                                // однажды кадр из эфира ложился на список свободных кадров.
                                // Ядро падало общей защитой в `frame::alloc`, читая вместо
                                // адреса кусок эфира, — на запуске программы, потому что запуск
                                // берёт памяти много. Драйвер, который гасит своё железо, —
                                // обязанность драйвера ([`rt2800::stop`]), но ЗАБЫТЬ её не
                                // должно стоить системе целостности памяти.
                                //
                                // Цена — страницы не возвращаются вовсе (свободного места
                                // меньше на размер кольца). Это честный размен: течь памяти
                                // видна и ограничена, а порча — ни то ни другое.
                                for i in 0..pages {
                                    ok &= unsafe {
                                        arch::map(root, va + i * PAGE, pa + i * PAGE,
                                                  arch::MAP_R | arch::MAP_W | arch::MAP_U
                                                      | arch::MAP_SHARED)
                                    };
                                    if !ok {
                                        break;
                                    }
                                }
                                flush_space(t, t.procs[cur].space);
                                if ok {
                                    pa // физ-адрес начала (драйверу нужен именно физический)
                                } else {
                                    // Веха 89: нет памяти под таблицу — отказ. Куски отдаём
                                    // поштучно: непрерывность нужна была карте, а не котлу.
                                    for i in 0..pages {
                                        frame::free(pa + i * PAGE);
                                    }
                                    usize::MAX
                                }
                            }
                            None => usize::MAX,
                        }
                    } else {
                        usize::MAX
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_DMA_ALLOC отклонён: {:?}  ← нет cap на DMA", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_IRQ_WAIT(irq_cap, timeout_ns) -> 0 | MAX (Веха 52): усыпить userspace-драйвер до
        // прерывания его устройства (нужен Irq-cap). Кадр продвигаем СЕЙЧАС (вернётся 0 при
        // пробуждении); процесс уходит в IrqWait, планировщик даёт ход другим. Разбудит
        // drain_userdrv_irq по флагу от обработчика VEC_USERDRV. Уже пришедший IRQ поймает drain
        // в resume() сразу — потери нет.
        //
        // **Веха 195 — СРОК** (`timeout_ns`, 0 — ждать вечно, как было). Без него драйвер с
        // таймерами не принимал ни одного кадра, и это стоило целого захода: кооперативный
        // планировщик шима спит на прерывании ТОЛЬКО когда у него нет ни одного срока, а у
        // живого драйвера срок есть всегда (сторож, проверка линка, watchdog). Он уходил спать
        // по времени, обработчик прерывания не звался, NAPI не планировался, `poll` не вынимал
        // кадры из кольца — снаружи это выглядело как «карта поднята, сеть не работает».
        //
        // Возврат по сроку — тот же 0, что по прерыванию, и это не небрежность: драйвер обязан
        // сверяться с регистром причин, а не верить, что его будят только по делу (в Linux это
        // то же правило разделяемой линии).
        33 => {
            let (icap, timeout_ns) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1) as u64)
            };
            let dom = t.procs[cur].domain;
            match cap::irq(dom, Cap::from_bits(icap as u64), Rights::READ) {
                Ok(_vector) => {
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(0);
                    f.advance();
                    t.procs[cur].state = State::IrqWait;
                    t.procs[cur].futex_deadline = (timeout_ns > 0)
                        .then(|| arch::now_ticks() + crate::clock::ns_to_ticks(timeout_ns));
                    // Веха 52 — «взвести» линию (размаскировать в IOAPIC): если причина уже
                    // висит на карте, прерывание доставится сразу; обработчик снова замаскирует.
                    arch::userdrv_irq_arm();
                    if let Some(n) = t.next_runnable(cur) {
                        t.set_cur(n);
                    }
                }
                Err(e) => {
                    vprintln!("  [drv] P{} SYS_IRQ_WAIT отклонён: {:?}  ← нет cap на IRQ", cur, e);
                    let f = &mut t.procs[cur].frame;
                    f.set_ret(usize::MAX);
                    f.advance();
                }
            }
        }
        // SYS_OBJ_LIST_ROOTS(store_cap, buf_ptr, buf_len) -> ПОЛНАЯ длина текста | MAX:
        // перечислить СЫРЫЕ корни store текстом («короткий id + имя» на строку) — vsh `roots`,
        // как `ls` для объектов store. Гейт: store-cap с READ ИЛИ WRITE (любой из
        // привилегированных доступов к store позволяет узнать имена корней; у shell'а cap
        // store:xw — есть WRITE).
        //
        // Веха 107: возвращается длина ВСЕГО текста, а не записанного. Раньше отдавалось
        // `min(длина, буфер)` — и «корней ровно столько» было не отличить от «буфер мал», причём
        // обрезание приходилось на середину строки: имя корня доезжало покалеченным. На этом
        // стоит нумерация поколений (`system/gen<N>`, `pkg/profile/*/gen<N>`), а `pkg` заводит
        // по два корня на каждый путь замыкания — недосчитаться поколения значило бы ЗАТЕРЕТЬ
        // существующее. Соглашение то же, что у SYS_OBJ_CHILDREN и readdir персоналии.
        34 => {
            let (scap, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let cap = Cap::from_bits(scap as u64);
            let allowed = cap::store(dom, cap, Rights::READ).is_ok()
                || cap::store(dom, cap, Rights::WRITE).is_ok();
            let result = if !allowed {
                vprintln!("  [obj] P{} OBJ_LIST_ROOTS отклонён ← нет capability (READ/WRITE) на store", cur);
                usize::MAX
            } else if ensure_heap_range(t, cur, bptr, blen) {
                let text = crate::object::list_roots_text();
                let bytes = text.as_bytes();
                let n = bytes.len().min(blen);
                let dst = unsafe { core::slice::from_raw_parts_mut(bptr as *mut u8, n) };
                dst.copy_from_slice(&bytes[..n]);
                vprintln!(
                    "  [obj] P{} OBJ_LIST_ROOTS → {} Б из {} ({} корней)",
                    cur, n, bytes.len(), text.lines().count()
                );
                bytes.len()
            } else {
                usize::MAX // куча под буфер не доотобразилась
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GC(store_cap) -> собрано объектов | MAX: сборка мусора store по достижимости
        // от корней (Веха 109). Нужен store-cap с WRITE: это операция, меняющая store.
        //
        // Наружу она понадобилась пакетам: `pkg gc` снимает корни путей, выпавших из всех
        // поколений профиля, — но пока никто не пройдёт по графу, место занято по-прежнему.
        // Раньше сборка случалась только на загрузке, то есть «удалил — перезагрузись».
        46 => {
            let scap = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => {
                    let (kept, collected) = crate::object::gc();
                    println!(
                        "  [gc] P{} по запросу: достижимо {}, собрано {}",
                        cur, kept, collected
                    );
                    collected
                }
                Err(e) => {
                    vprintln!("  [obj] P{} SYS_OBJ_GC отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SLEEP(ns) -> 0 (Веха 114): уснуть на указанное время. Прав не требует — спящий
        // ничего не делает и ничего не узнаёт; отказать ему значило бы заставить программу
        // крутить `yield` в пустом цикле, чем она до сих пор и занималась.
        //
        // Срок хранится в ТИКАХ, как у `futex_wait` и `recv_timeout`, и будит его та же
        // [`wake_futex_timeouts`]; наносекунды переводятся ЗДЕСЬ, чтобы программе не надо было
        // знать таймбазу своей архитектуры. `ns == 0` — не спать, просто уступить процессор.
        47 => {
            let ns = t.procs[cur].frame.arg(0) as u64;
            if ns == 0 {
                // Возврат оформляем сами: спать не будем, а значит и будить некому.
                let f = &mut t.procs[cur].frame;
                f.set_ret(0);
                f.advance();
            } else {
                // sepc НЕ двигаем и ret не ставим — это сделает пробуждение по сроку
                // ([`wake_futex_timeouts`]), ровно как у `futex_wait`.
                let deadline = arch::now_ticks().wrapping_add(crate::clock::ns_to_ticks(ns));
                t.procs[cur].state = State::Sleeping;
                t.procs[cur].futex_deadline = Some(deadline);
            }
            if let Some(n) = t.next_runnable(cur) {
                t.set_cur(n);
            }
        }
        // SYS_PARENT(pid) -> ppid | MAX (Веха 114): чей это ребёнок.
        //
        // Понадобилось мультиплексору. Он раздаёт панелям своё право на stdio, ребёнок панели
        // (шелл) наследует его дальше — и внук пишет НАМ, но под своим номером процесса. Пока
        // вывод раскладывался по панелям сравнением «отправитель == ребёнок панели», всё, что
        // шелл запускал, печаталось В НИКУДА: `pkg update` честно отработал десять минут и не
        // показал ни строки. Теперь хост поднимается по родителям и находит владельца.
        //
        // Гейта прав нет намеренно: номер процесса и так не тайна (его возвращает `SYS_SPAWN`,
        // он приходит в каждом сообщении), а родство — то же самое знание, только на шаг выше.
        // Изменить оно ничего не даёт: убить и дождаться по-прежнему можно лишь СВОЕГО ребёнка.
        48 => {
            let pid = t.procs[cur].frame.arg(0);
            let ppid = t
                .procs
                .get(pid)
                .map(|p| p.parent)
                .filter(|&pp| pp != usize::MAX)
                .unwrap_or(usize::MAX);
            let f = &mut t.procs[cur].frame;
            f.set_ret(ppid);
            f.advance();
        }
        // SYS_MOUSE_READ(buf, len) -> байт | MAX (Веха 115): забрать накопившиеся события мыши.
        //
        // Событие — 6 байт: dx (i16 LE), dy (i16 LE), кнопки (бит0 левая, бит1 правая, бит2
        // средняя), и байт под будущее (колесо). Возвращается ЧИСЛО БАЙТ, чтобы читатель не
        // гадал, сколько событий влезло.
        //
        // Право — ВЛАДЕНИЕ ЭКРАНОМ, а не отдельная capability. Довод простой: курсор существует
        // только на экране, и тот, кто экраном не владеет, не может ни нарисовать его, ни
        // осмысленно ответить на клик. Заодно это закрывает подслушивание: пока терминал держит
        // экран, чужой процесс не прочитает, что человек делает мышью.
        49 => {
            let (buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let mine = arch::video_owner() == Some(cur);
            let result = if !mine {
                usize::MAX
            } else if !ensure_heap_range(t, cur, buf, len) {
                usize::MAX
            } else {
                let mut off = 0usize;
                while off + 6 <= len {
                    let Some(e) = arch::mouse_pop() else { break };
                    let bytes = [
                        e.dx.to_le_bytes()[0], e.dx.to_le_bytes()[1],
                        e.dy.to_le_bytes()[0], e.dy.to_le_bytes()[1],
                        e.buttons, e.wheel as u8,
                    ];
                    let dst = unsafe { core::slice::from_raw_parts_mut((buf + off) as *mut u8, 6) };
                    dst.copy_from_slice(&bytes);
                    off += 6;
                }
                let lost = arch::mouse_take_lost();
                if lost > 0 {
                    println!("  [мышь] потеряно событий: {} (владелец не успевает читать)", lost);
                }
                off
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_KEY_READ(buf, len) -> байт (Веха 119): события клавиатуры для владельца экрана.
        //
        // Событие — 6 байт: код клавиши (u16 LE), маска модификаторов, флаги (бит0 — нажата),
        // готовый ASCII-байт и запас. Право то же, что у мыши: ВЛАДЕНИЕ ЭКРАНОМ. Довод тот же и
        // он же закрывает подслушивание — пока окнами занят один процесс, чужой не прочитает,
        // что человек печатает.
        51 => {
            let (buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let result = if arch::video_owner() != Some(cur) || !ensure_heap_range(t, cur, buf, len)
            {
                usize::MAX
            } else {
                let mut off = 0usize;
                while off + 6 <= len {
                    let Some(e) = arch::key_pop() else { break };
                    let b = [
                        e.sym.to_le_bytes()[0], e.sym.to_le_bytes()[1],
                        e.mods, e.down as u8,
                        e.ch.to_le_bytes()[0], e.ch.to_le_bytes()[1],
                    ];
                    let dst = unsafe { core::slice::from_raw_parts_mut((buf + off) as *mut u8, 6) };
                    dst.copy_from_slice(&b);
                    off += 6;
                }
                off
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SETENV(buf, len) -> 0 / MAX (Веха 120.1): заменить СВОЁ окружение (`KEY=VAL\0…`).
        //
        // Зачем понадобилось: у программ VOID нет текущего каталога — его ведёт шелл, и до сих
        // пор он не мог сообщить его ребёнку никак. `ved terminal.vv` после `cd /etc/system`
        // открывал не тот файл (пустой, по пути `/terminal.vv`), а сохранение создало бы мусор.
        // Теперь `cd` кладёт `CWD=` себе в окружение, а дети наследуют его вместе с остальным.
        //
        // Прав это НЕ раздаёт, и вот почему. Окружение — слой ИМЁН над таблицей стартовых
        // capability, а сама таблица наследуется целиком и неизменной. Переписав `CAP_STORE=3`,
        // процесс укажет ребёнку на другое СВОЁ право — то, которое ребёнок и так получил.
        // Соврать про чужое право нельзя: его в таблице нет.
        53 => {
            let (buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let ok = len <= ARGS_MAX && (len == 0 || ensure_heap_range(t, cur, buf, len));
            if ok {
                let src = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                let env = src.to_vec();
                // Веха 35: окружение — свойство процесса, живёт у лидера группы нитей.
                let leader = t.procs[cur].group;
                t.procs[leader].env = env;
            }
            let f = &mut t.procs[cur].frame;
            f.set_ret(if ok { 0 } else { usize::MAX });
            f.advance();
        }
        // SYS_SHM_NEW(len, va) -> биты права | MAX (Веха 129): создать ОБЩУЮ ОБЛАСТЬ на `len`
        // байт, отобразить её себе по `va` (U|R|W) и вернуть право на неё.
        //
        // Адрес выбирает ВЫЗЫВАЮЩИЙ — как у `SYS_DMA_ALLOC`. Ядро проверяет только границы
        // пользовательской области; попасть в собственный образ или кучу — забота процесса, и
        // на это уже наступили при первом же опыте (область легла на 0x4000_0000, где лежит код
        // самой программы, и процесс убил себя). Библиотека держит для этого своё окно.
        //
        // Право потом передаётся по IPC тому, с кем делятся буфером ([[shm]]). Никакого
        // «глобального имени области» нет и не будет: единственный способ её получить — принять
        // право, а подделать его нельзя. Так «этот процесс видит буфер того окна» становится
        // проверяемым фактом.
        54 => {
            let (len, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let result = shm_map_new(t, cur, len, va);
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SHM_MAP(shm_cap, va) -> длина | MAX (Веха 129): отобразить у себя ТЕ ЖЕ страницы.
        // Права cap решают режим: без WRITE область ложится только на чтение — композитору
        // хватает чтения, и давать ему больше незачем.
        55 => {
            let (scap, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let c = Cap::from_bits(scap as u64);
            let result = match cap::shm(dom, c, Rights::READ) {
                Ok(id) => {
                    let writable = cap::rights(dom, c).map(|r| r.contains(Rights::WRITE)).unwrap_or(false);
                    shm_map_existing(t, cur, id, va, writable)
                }
                Err(e) => {
                    vprintln!("  [shm] P{} SYS_SHM_MAP отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SHM_UNMAP(shm_cap, va) -> 0 | MAX (Веха 129): отпустить область — снять её со
        // своих адресов и убавить держателя. Ушёл последний — страницы вернулись в общий котёл.
        //
        // Право нужно то же, что на отображение: оно называет область, а не даёт власть над ней.
        // Отпустить чужое отображение через него нельзя — снимается только своё (см. `shm_unmap`).
        56 => {
            let (scap, va) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::shm(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(id) => shm_unmap(t, cur, id, va),
                Err(e) => {
                    vprintln!("  [shm] P{} SYS_SHM_UNMAP отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_KEYMAP(op) -> раскладка (0 — US, 1 — RU) | MAX (Веха 143).
        //
        // `op = 0` — спросить, `op = 1` — следующая. Таблицы скан-кодов живут в драйвере, и
        // вторая раскладка в userspace неминуемо разошлась бы с этой; но АККОРД переключения —
        // дело конфига, а не драйвера, поэтому переключение вынесено сюда.
        //
        // Право: переключать может тот, кому принадлежит ЭКРАН. Отдельной capability не заводим —
        // раскладка не ресурс, а состояние сеанса, и владелец сеанса ровно тот, кто на этом экране
        // рисует (композитор в оконном режиме, полноэкранный терминал в текстовом). Спросить может
        // кто угодно: язык ввода не секрет, а панели он нужен для показа.
        57 => {
            let op = t.procs[cur].frame.arg(0);
            let owner = arch::video_owner();
            let mine = owner == Some(cur) || owner == Some(t.procs[cur].group);
            let result = if op == 0 {
                arch::keymap()
            } else if mine {
                arch::keymap_set((arch::keymap() + 1) % arch::keymaps());
                arch::keymap()
            } else {
                usize::MAX
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
        // SYS_PROC_LIST(sysview_cap, buf_ptr, buf_len) -> число процессов | MAX (Веха 153):
        // перечислить ЖИВЫЕ процессы (лидеры групп — нить не отдельная программа) под правом
        // Sysview READ. Запись — 64 байта: pid u16, родитель u16 (0xFFFF — никто), флаги u16
        // (bit0 системный, bit1 linux, bit2 есть-хэш), состояние u8, длина имени u8, content-id
        // образа [32] и имя [24] (что ИМЕННО исполняется — хэш; имя рядом лишь для человека,
        // удостоверением оно у нас не является, [[task-manager]]). Возвращается ПОЛНОЕ число
        // процессов: если оно больше buf_len/64, клиент недосчитался и перезапросит бо́льшим
        // буфером (уловка SYS_OBJ_LIST_ROOTS). Без cap ядро молчит — ambient-доступа к списку нет.
        59 => {
            let (scap, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::sysview(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) if ensure_heap_range(t, cur, bptr, blen) => {
                    const REC: usize = 64;
                    let cap_recs = blen / REC;
                    let mut written = 0usize;
                    let mut total = 0usize;
                    for i in 0..t.procs.len() {
                        let p = &t.procs[i];
                        // Мёртвый слот или НИТЬ (делит домен/образ лидера) — не отдельная строка.
                        if p.state == State::Finished || p.group != i {
                            continue;
                        }
                        total += 1;
                        if written >= cap_recs {
                            continue; // буфер полон — досчитываем total, но не пишем
                        }
                        let mut rec = [0u8; REC];
                        rec[0..2].copy_from_slice(&(i as u16).to_le_bytes());
                        let parent =
                            if p.parent == usize::MAX { 0xFFFFu16 } else { p.parent as u16 };
                        rec[2..4].copy_from_slice(&parent.to_le_bytes());
                        let mut flags = 0u16;
                        if p.system { flags |= 0x01; }
                        if p.linux { flags |= 0x02; }
                        if p.image.is_some() { flags |= 0x04; }
                        rec[4..6].copy_from_slice(&flags.to_le_bytes());
                        rec[6] = match p.state {
                            State::Runnable => 0,
                            State::RecvWait => 1,
                            State::ReplyWait => 2,
                            State::StdinWait => 3,
                            State::ExecWait(_) => 4,
                            State::JoinWait(_) => 5,
                            State::FutexWait => 6,
                            State::IrqWait => 7,
                            State::Sleeping => 8,
                            State::Finished => 9,
                            State::PipeWait(_) => 10,
                            State::ChildWait(_) => 11,
                        };
                        // Имя = argv[0] до NUL, обрезанное до 24 байт.
                        let name: &[u8] = p.args.split(|&b| b == 0).next().unwrap_or(&[]);
                        let nlen = name.len().min(24);
                        rec[7] = nlen as u8;
                        if let Some(ContentId(id)) = p.image {
                            rec[8..40].copy_from_slice(&id);
                        }
                        rec[40..40 + nlen].copy_from_slice(&name[..nlen]);
                        let dst = unsafe {
                            core::slice::from_raw_parts_mut((bptr + written * REC) as *mut u8, REC)
                        };
                        dst.copy_from_slice(&rec);
                        written += 1;
                    }
                    vprintln!("  [proc] P{} PROC_LIST → {} из {} (по cap)", cur, written, total);
                    total
                }
                Ok(()) => usize::MAX, // право есть, а фреймов под буфер нет
                Err(_) => usize::MAX, // нет права Sysview — молчим
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_PROC_CAPS(sysview_cap, pid, buf_ptr, buf_len) -> число прав | MAX (Веха 153.2):
        // перечислить c-space процесса `pid` под правом Sysview READ — для ГРАФА «кто чей эндпоинт
        // держит». Важен не перечень прав (успокаивающая ложь), а СВЯЗИ: процесс без права на сеть
        // всё равно может позвать того, у кого оно есть (confused deputy). Запись — 12 байт: слот
        // u16, вид u8, _pad u8, права u32, aux u16 (id связанного процесса у Endpoint/Reply — ребро
        // графа; иначе 0xFFFF), _pad u16. Полное число прав; MAX — нет права обзора или неверный pid.
        60 => {
            let (scap, pid, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::sysview(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(())
                    if pid < t.procs.len()
                        && t.procs[pid].state != State::Finished
                        && ensure_heap_range(t, cur, bptr, blen) =>
                {
                    const REC: usize = 12;
                    let cap_recs = blen / REC;
                    let caps = cap::list_caps(t.procs[pid].domain);
                    let mut written = 0usize;
                    for &(slot, kind, rights, aux) in &caps {
                        if written >= cap_recs {
                            break;
                        }
                        let mut rec = [0u8; REC];
                        rec[0..2].copy_from_slice(&slot.to_le_bytes());
                        rec[2] = kind;
                        rec[4..8].copy_from_slice(&rights.to_le_bytes());
                        rec[8..10].copy_from_slice(&aux.to_le_bytes());
                        let dst = unsafe {
                            core::slice::from_raw_parts_mut((bptr + written * REC) as *mut u8, REC)
                        };
                        dst.copy_from_slice(&rec);
                        written += 1;
                    }
                    vprintln!(
                        "  [proc] P{} PROC_CAPS P{} → {} из {} прав",
                        cur, pid, written, caps.len()
                    );
                    caps.len()
                }
                Ok(()) => usize::MAX, // неверный pid / мёртвый / нет фреймов буфера
                Err(_) => usize::MAX, // нет права Sysview
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_PROC_STAT(sysview_cap, pid, buf_ptr) -> 0 | MAX (Веха 153.3): «что процесс делает
        // СЕЙЧАС» под правом Sysview READ. 64 Б в буфер: calls_made u64, bytes_sent u64,
        // calls_recv u64, bytes_recv u64, flags u16 (bit0 — держит ЭКРАН), резерв u16,
        // страниц кучи u32 (Веха 163), процессорное время в нс u64 (Веха 163), дальше резерв.
        // Резерв тут и пригодился: запись `PROC_LIST` занята целиком, а этой было куда расти. Это не
        // эвристика и не выборка (как «Диск 2%» снаружи), а учёт самих опосредованных вызовов —
        // правда по построению; скорость диспетчер считает вычитанием двух замеров.
        61 => {
            let (scap, pid, bptr) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::sysview(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(())
                    if pid < t.procs.len()
                        && t.procs[pid].state != State::Finished
                        && ensure_heap_range(t, cur, bptr, 64) =>
                {
                    let holds_screen = arch::video_owner() == Some(pid);
                    // Веха 163 — память считается У ЛИДЕРА ГРУППЫ: куча общая на все нити, и
                    // показать её каждой значило бы посчитать одни и те же страницы дважды.
                    let leader = t.procs[pid].group;
                    let pages = t.procs.get(leader).map_or(0, |l| l.pages) as u32;
                    let p = &t.procs[pid];
                    let run_ns = crate::clock::ticks_to_ns(p.run_ticks);
                    let mut rec = [0u8; 64];
                    rec[0..8].copy_from_slice(&p.calls_made.to_le_bytes());
                    rec[8..16].copy_from_slice(&p.bytes_sent.to_le_bytes());
                    rec[16..24].copy_from_slice(&p.calls_recv.to_le_bytes());
                    rec[24..32].copy_from_slice(&p.bytes_recv.to_le_bytes());
                    let flags: u16 = if holds_screen { 0x01 } else { 0 };
                    rec[32..34].copy_from_slice(&flags.to_le_bytes());
                    rec[36..40].copy_from_slice(&pages.to_le_bytes());
                    rec[40..48].copy_from_slice(&run_ns.to_le_bytes());
                    let dst = unsafe { core::slice::from_raw_parts_mut(bptr as *mut u8, 64) };
                    dst.copy_from_slice(&rec);
                    0
                }
                Ok(()) => usize::MAX, // неверный pid / мёртвый / нет фреймов буфера
                Err(_) => usize::MAX, // нет права Sysview
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_PROC_REVOKE(sysview_cap, pid, slot) -> 0 | MAX (Веха 153.4): отозвать право в слоте
        // c-space процесса `pid` — под правом Sysview WRITE (не READ: это ДЕЙСТВИЕ, а не обзор).
        // Скальпель вместо топора ([[task-manager]]): не «убить процесс», а «отобрать у него сеть
        // на ходу» — слот с endpoint→net-srv, и он теряет её немедленно (cap::revoke_slot бумкает
        // поколение → висящий дескриптор протухает). MAX — нет права / неверный pid / пустой слот.
        62 => {
            let (scap, pid, slot) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::sysview(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if pid < t.procs.len() && t.procs[pid].state != State::Finished => {
                    let tdom = t.procs[pid].domain;
                    if cap::revoke_slot(tdom, slot) {
                        vprintln!(
                            "  [proc] P{} PROC_REVOKE P{} слот {} — право отозвано",
                            cur, pid, slot
                        );
                        0
                    } else {
                        usize::MAX // слота нет или он пуст
                    }
                }
                _ => usize::MAX, // нет права Sysview WRITE или неверный pid
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SYSINFO(sysview_cap, buf_ptr, buf_len) -> 0 | MAX (Веха 159): числа ПРО МАШИНУ,
        // а не про процесс, под правом Sysview READ. 48 Б в буфер:
        //   0..8   всего памяти, байт          8..16  занято памяти, байт
        //   16..24 время с загрузки, нс        24..32 из него ПРОСТОЙ, нс
        //   32..34 живых процессов, u16        34..36 ядер у машины, u16
        //   36..38 ядер поднято, u16            38..40 ядер под планировщиком, u16
        //   40..48 резерв (нули)
        //
        // Загрузка процессора отсюда считается ВЫЧИТАНИЕМ двух замеров: `1 - Δпростой/Δвремя`.
        // Мгновенного числа ядро не отдаёт намеренно — «сейчас» у загрузки не бывает, бывает
        // только «за промежуток», и выбирать промежуток должен тот, кто рисует.
        //
        // Под правом, а не свободно (в отличие от `SYS_TIME`): сколько памяти занято и сколько
        // машина простаивает — это наблюдение за системой, то же самое, что список процессов.
        // Панель получает право обзора так же, как диспетчер, — строкой конфига.
        63 => {
            let (scap, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::sysview(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) if blen >= SYSINFO_REC && ensure_heap_range(t, cur, bptr, blen) => {
                    // Нить отдельным процессом не считается — тот же уговор, что у PROC_LIST.
                    let live = (0..t.procs.len())
                        .filter(|&i| t.procs[i].state != State::Finished && t.procs[i].group == i)
                        .count()
                        .min(0xffff);
                    let mut rec = [0u8; SYSINFO_REC];
                    rec[0..8].copy_from_slice(&(crate::frame::usable_bytes() as u64).to_le_bytes());
                    rec[8..16].copy_from_slice(&(crate::frame::used_bytes() as u64).to_le_bytes());
                    rec[16..24].copy_from_slice(&crate::clock::uptime_ns().to_le_bytes());
                    let idle = crate::clock::ticks_to_ns(idle_ticks_avg());
                    rec[24..32].copy_from_slice(&idle.to_le_bytes());
                    rec[32..34].copy_from_slice(&(live as u16).to_le_bytes());
                    // Веха 170 — ЯДРА: сколько машина объявила и сколько из них поднято нами.
                    // Два числа, а не одно: «ядер 4» на системе, работающей на одном, — это
                    // неправда ровно в ту сторону, в которую соврать соблазнительнее всего.
                    rec[34..36]
                        .copy_from_slice(&(crate::arch::cpu_count().min(0xffff) as u16).to_le_bytes());
                    rec[36..38]
                        .copy_from_slice(&(crate::arch::cpus_up().min(0xffff) as u16).to_le_bytes());
                    rec[38..40].copy_from_slice(&sched_cores().to_le_bytes());
                    let dst = unsafe { core::slice::from_raw_parts_mut(bptr as *mut u8, SYSINFO_REC) };
                    dst.copy_from_slice(&rec);
                    0
                }
                _ => usize::MAX, // нет права Sysview READ, тесный буфер или он не наш
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_PCI_CFG(mmio_cap, off, val, write) -> слово | MAX (Веха 199.11): настоящее
        // КОНФИГУРАЦИОННОЕ ПРОСТРАНСТВО своего устройства.
        //
        // Зачем. Хостируемые драйверы Linux ходят в конфиг PCI постоянно и по делу:
        // `pci_set_master` (без него нет DMA), MSI, ASPM, управление питанием, маскирование
        // ошибок PCIe. У нас всё это уходило в МАССИВ В ПАМЯТИ ПРОЦЕССА — шим держал свой
        // `lx_config[]` и честно возвращал записанное, так что драйвер видел успех, а железо не
        // менялось ни на бит. Худший вид заглушки: она не отказывает, она соглашается.
        //
        // Отдельного права на «конфиг PCI» нет и не нужно: `mmio:<имя>` уже означает владение
        // устройством, а конфиг — такая же его часть, как BAR. Связь между правом и устройством
        // даёт САМА БАЗА окна, которую ядро и выдало (`pci_bdf_by_bar`), поэтому подменить
        // устройство вызывающий не может: он назовёт своё право, а BDF найдёт ядро.
        //
        // Смещение ограничено 256 байтами (механизм 0xCF8) и выравнено по слову.
        65 => {
            let (mcap, off, val, write) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::mmio(dom, Cap::from_bits(mcap as u64), Rights::READ) {
                Ok((base, _len)) if off < 256 && off % 4 == 0 => cfg_pci(base, off, val, write),
                _ => usize::MAX,
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_HWPROBE(hw_cap, kind, a, b, value, write) -> слово | MAX (Веха 200): прочитать или
        // записать регистр устройства — окно MMIO (kind 0) либо конфигурацию PCI (kind 1).
        //
        // Зачем это в системе. Фаза драйверов (Вехи 191–199) шла циклами «пересобрал → записал
        // на флешку → загрузился → отправил журнал», и раз за разом выяснялось, что не хватает
        // ОДНОГО ЧИСЛА из регистра. Каждое такое число стоило круга. С этим вызовом оно
        // спрашивается на живой машине одной строкой в шелле.
        //
        // Право отдельное (`hwprobe`), а не часть `sysview`: обзор процессов и доступ к
        // регистрам устройств — разные виды власти, и складывать их значило бы, что диспетчер
        // задач умеет останавливать контроллеры. `READ` и `WRITE` тоже разделены: чтение
        // регистра редко что-то меняет, запись способна остановить устройство.
        66 => {
            let (hcap, kind, a, b, val, write) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let dom = t.procs[cur].domain;
            let need = if write == 0 { Rights::READ } else { Rights::WRITE };
            let result = match cap::hwprobe(dom, Cap::from_bits(hcap as u64), need) {
                Ok(()) => hw_probe(kind, a, b, val, write),
                Err(e) => {
                    vprintln!("  [hw] P{} SYS_HWPROBE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_CONSIZE() -> (колонок, строк) (Веха 120): размер КОНСОЛИ ЯДРА в знакоместах.
        //
        // Нужен ровно там, где нет хоста stdio: программа, рисующая во весь экран (`bin/ved`),
        // спрашивает размер у своего терминала — а в спасательном шелле терминала нет, есть
        // консоль ядра. Вывести её геометрию снаружи нельзя: тот же экран бывает текстовым 80×25
        // и пиксельным 160×50, и знает об этом только ядро.
        //
        // Гейта прав нет — по тому же доводу, что у `SYS_TIME`: размер экрана не секрет и ничего
        // не меняет. Нули значат «не знаю» (serial-консоль riscv), и это ЧЕСТНЕЕ выдумки: врать
        // о размере хуже, чем промолчать, — программа возьмёт своё умолчание.
        52 => {
            let (cols, rows) = arch::console_size();
            let f = &mut t.procs[cur].frame;
            f.set_ret(cols);
            f.set_ret_at(1, rows);
            f.advance();
        }
        // SYS_KLOG(buf, len) -> байт (Веха 116): отдать журнал ядра — то, что оно печатало.
        //
        // Гейта прав нет, как у `SYS_LOG` и `SYS_TIME`: журнал — это то, что и так шло на экран,
        // а на машине без COM-порта он единственный способ ПЕРЕЧИТАТЬ увиденное. Секретов ядро
        // в него не кладёт; если однажды положит — гейт появится вместе с ними, а не заранее.
        //
        // Если буфер меньше журнала, отдаются ПОСЛЕДНИЕ байты: при разборе неполадки ценнее
        // свежее. Сколько потеряно кольцом, читатель узнаёт вторым значением.
        50 => {
            let (buf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let (n, lost) = if !ensure_heap_range(t, cur, buf, len) {
                (usize::MAX, 0)
            } else {
                let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
                (klog::read(out), klog::lost())
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(n);
            f.set_ret_at(1, lost);
            f.advance();
        }
        // SYS_LOG(on) -> 0: вкл/выкл подробный трейс ядра (vprintln — [ipc]/[obj]/[mm]/[exec]/…).
        // Отладочная удобность, не привилегия (гейта нет): по умолчанию интерактивная сессия тихая,
        // чтобы трейс не сбивал вывод команд; `log on` в шелле включает обратно.
        35 => {
            let on = t.procs[cur].frame.arg(0) != 0;
            set_verbose(on);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_TIME(kind) -> наносекунды (Веха 86). kind: 0 = настенное время Unix (UTC),
        // 1 = монотонное с загрузки. Гейта прав нет — время не секрет и ничего не меняет
        // (как SYS_LOG). Наносекунды влезают в usize: обе арх 64-битные (u64 хватит до 2554 года).
        //
        // Веха 136: kind = 2 — ТАЙМБАЗА, тиков в секунду. Программа читает счётчик сама (`rdtsc`
        // /`rdtime` открыты в U-mode ради дешёвых замеров), а цену тика знать обязана: раньше она
        // была константой в каждой программе («x86 ≈ 1 ГГц»), и когда ядро научилось эту частоту
        // измерять, userspace продолжил бы считать по-старому. Одна таймбаза на систему — та,
        // которую измерило ядро.
        36 => {
            let kind = t.procs[cur].frame.arg(0);
            let v = match kind {
                1 => crate::clock::uptime_ns(),
                2 => crate::clock::tick_hz(),
                _ => crate::clock::realtime_ns(),
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(v as usize);
            f.advance();
        }
        // SYS_RANDOM(buf, len) -> len | MAX (Веха 86): заполнить буфер процесса случайными
        // байтами (аппаратный ГСЧ + пул событий, см. [`crate::random`]). Буфер может лежать в
        // ленивой куче — доотображаем, как в SYS_WRITE.
        37 => {
            let (ptr, len, kind) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            // Веха 95: `kind == 1` — не выдача байт, а ВОПРОС «есть ли сильный источник».
            // Нужен TLS: строить ключи на пуле джиттера без подтверждённого источника нельзя,
            // и решать это должен потребитель, а не молча ядро.
            if kind == 1 {
                let strong = crate::random::has_strong_source();
                let f = &mut t.procs[cur].frame;
                f.set_ret(strong as usize);
                f.advance();
                return;
            }
            let result = if len == 0 {
                0
            } else if ensure_heap_range(t, cur, ptr, len) {
                let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) };
                crate::random::fill(out);
                len
            } else {
                usize::MAX
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT_NODE(store_cap, buf, len, kids_ptr, nkids, idout) -> 0|MAX (Веха 94):
        // положить УЗЕЛ — значение плюс список исходящих ссылок (по 32 байта каждая).
        //
        // Зачем отдельно от `SYS_OBJ_PUT`: большой файл не кладётся одним слайсом — ни в кучу
        // процесса, ни в кучу ядра. Он кладётся КУСКАМИ (каждый — обычный объект), а узел
        // связывает их в целое. Дедуп при этом достаётся даром: одинаковый кусок в двух
        // загрузках — один объект. GC уже умеет ходить по детям (checkpoint строит такое же
        // дерево с Вехи 37), так что новой машинерии не появляется — только доступ из userspace.
        38 => {
            let (scap, buf, len, kids, nkids, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let dom = t.procs[cur].domain;
            let kbytes = nkids.saturating_mul(32);
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && (nkids == 0 || ensure_heap_range(t, cur, kids, kbytes))
                    && ensure_heap_range(t, cur, idout, 32) =>
                {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    let mut children = Vec::with_capacity(nkids);
                    for i in 0..nkids {
                        let mut id = [0u8; 32];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (kids + i * 32) as *const u8, id.as_mut_ptr(), 32,
                            )
                        };
                        children.push(void_abi::ContentId(id));
                    }
                    // Веха 104 — нехватка памяти ядра: отказ, а не паника (см. OBJ_PUT).
                    match crate::object::try_put_node(bytes, &children) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт + {} детей → content-id (по cap)",
                                cur, len, nkids,
                            );
                            0
                        }
                        None => {
                            println!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт: НЕ ХВАТИЛО памяти ядра",
                                cur, len,
                            );
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT_NODE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_CHILDREN(store_cap, id_ptr, out_buf, out_cap) -> число детей | MAX (Веха 94):
        // выписать ссылки узла (по 32 байта). Без этого положенное деревом нельзя прочитать
        // обратно: `SYS_OBJ_GET` отдаёт только полезную нагрузку узла, а не его детей.
        39 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) if ensure_heap_range(t, cur, idp, 32)
                    && (ocap == 0 || ensure_heap_range(t, cur, obuf, ocap)) =>
                {
                    let mut id = [0u8; 32];
                    unsafe { core::ptr::copy_nonoverlapping(idp as *const u8, id.as_mut_ptr(), 32) };
                    let kids = crate::object::children(&void_abi::ContentId(id));
                    let n = kids.len().min(ocap / 32);
                    for (i, c) in kids.iter().take(n).enumerate() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                c.0.as_ptr(), (obuf + i * 32) as *mut u8, 32,
                            )
                        };
                    }
                    // Возвращаем ПОЛНОЕ число детей, а не сколько влезло: иначе вызывающий не
                    // отличил бы «детей ровно столько» от «буфер мал» и потерял бы хвост.
                    kids.len()
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_CHILDREN отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_VIDEO_INFO(mmio_cap, out) -> 0 | MAX (Веха 97): описание видеорежима в буфер
        // процесса — 10 × u32: ширина, высота, шаг строки, бит/пиксель и по паре
        // (позиция, ширина маски) на R, G, B.
        //
        // Права те же, что на само окно: числа сами по себе безобидны, но отдавать их отдельно
        // от права рисовать незачем — так геометрия неотделима от capability, а не висит
        // «общедоступной справкой» рядом с ней.
        40 => {
            let (mcap, out) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let dom = t.procs[cur].domain;
            const N: usize = 10;
            let result = match cap::mmio(dom, Cap::from_bits(mcap as u64), Rights::READ) {
                Ok(_) if ensure_heap_range(t, cur, out, N * 4) => {
                    let (w, h, pitch, bpp, rgb) = arch::video_info();
                    let vals: [u32; N] = [
                        w as u32, h as u32, pitch as u32, bpp as u32,
                        rgb[0].0 as u32, rgb[0].1 as u32,
                        rgb[1].0 as u32, rgb[1].1 as u32,
                        rgb[2].0 as u32, rgb[2].1 as u32,
                    ];
                    for (i, v) in vals.iter().enumerate() {
                        unsafe { core::ptr::write_unaligned((out + i * 4) as *mut u32, *v) };
                    }
                    0
                }
                Ok(_) => usize::MAX,
                Err(e) => {
                    vprintln!("  [видео] P{} VIDEO_INFO отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_SELF_ENDPOINT() -> cap (Веха 98): право ВЫЗЫВАТЬ этот процесс, чтобы отдать его
        // детям. Не расширение полномочий: принимать сообщения процесс может и так (`SYS_RECV`),
        // а кому раздать право на себя — его собственное дело. Без этого хост чужого stdio
        // невозможен: ребёнку некуда слать вывод, потому что сослаться на родителя нечем.
        //
        // Права SEND — только «позвать»; ни принимать за нас, ни раздавать дальше (нет GRANT).
        43 => {
            let dom = t.procs[cur].domain;
            let leader = t.procs[cur].group; // эндпоинт принадлежит ПРОЦЕССУ, не нити
            let cap = cap::mint(dom, cap::Target::Endpoint(leader), Rights::SEND);
            let f = &mut t.procs[cur].frame;
            f.set_ret(cap.bits() as usize);
            f.advance();
        }
        // SYS_KILL(pid) (Веха 103) — завершить СВОЕГО ребёнка, запущенного `SYS_SPAWN`.
        //
        // Право берётся оттуда же, откуда его берёт `SYS_WAIT`: из РОДИТЕЛЬСТВА. Отдельной
        // capability заводить не стали — она бы дублировала уже существующее отношение: кто
        // процесс создал, тот им и распоряжается, чужого не тронуть. Ровно этого не хватало,
        // чтобы закрытая панель мультиплексора не оставляла сироту ([[multiplexer]]).
        //
        // Код выхода — 137 (128+9), как принято для «убит», чтобы родитель отличал его от
        // обычного возврата.
        45 => {
            let pid = t.procs[cur].frame.arg(0);
            let ok = pid < t.procs.len()
                && pid != cur
                && t.procs[pid].parent == cur
                && t.procs[pid].state != State::Finished;
            if !ok {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            let leader = t.procs[pid].group;
            vprintln!("  [proc] P{} SYS_KILL P{} (свой ребёнок)", cur, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                    lx_close_all(t, i);
                }
            }
            crate::net::ext_detach(leader); // Веха 195: карта ушла с процессом
            // Ждущие узнают код выхода тем же путём, что и при обычном завершении; права на
            // мертвеца отзовёт `reclaim_dead_spaces` (Веха 89), когда освободит его слот.
            wake_exec_waiters(t, leader, 137);
            let f = &mut t.procs[cur].frame;
            f.set_ret(0);
            f.advance();
        }
        // SYS_POWEROFF(cap) (Веха 101) — выключить машину. Право отдельное (`Target::Power`,
        // токен `power` в конфиге): выключение — одностороннее действие над ВСЕЙ системой, и
        // «может любой процесс» здесь было бы дырой ровно того сорта, который capability-модель
        // и должна закрывать.
        //
        // Раньше выключения не было вовсе: `exit` в шелле лишь заканчивал программу, а машина
        // продолжала работать — сессия в нынешнем виде не кончается никогда (после ухода шелла
        // остаются сервисы, и `run()` честно крутит их дальше).
        44 => {
            let (dom, ccap) = (t.procs[cur].domain, t.procs[cur].frame.arg(0));
            // Веха 197 — ВТОРОЙ аргумент: 0 выключить, 1 перезагрузить. Право то же и по той же
            // причине: и то и другое — одностороннее действие над всей системой, разница лишь в
            // том, поднимется ли она обратно. Отдельного права на ребут заводить не за что.
            let restart = t.procs[cur].frame.arg(1) == 1;
            if !cap::may_power_off(dom, Cap::from_bits(ccap as u64)) {
                let f = &mut t.procs[cur].frame;
                f.set_ret(usize::MAX);
                f.advance();
                return;
            }
            println!(
                "  [power] {} по запросу процесса",
                if restart { "перезагрузка" } else { "выключение" },
            );
            // Синк ПЕРЕД снятием питания: иначе выключение съело бы хвост несинхронизированных
            // операций (окно group commit ~2 с).
            crate::object::commit();
            println!(
                "  [store] финальный синк: поколение {} · записано за сессию: {} КиБ",
                crate::object::generation(),
                crate::object::bytes_written() / 1024,
            );
            if restart {
                arch::reboot();
            }
            arch::power_off();
        }
        // SYS_WAIT(pid, nonblock) -> код выхода | WOULD_BLOCK | MAX (Веха 98): забрать результат
        // СВОЕГО ребёнка, запущенного `SYS_SPAWN`. Чужих детей ждать нельзя — иначе один процесс
        // мог бы наблюдать за жизнью другого, ничего на него не имея.
        //
        // `nonblock` — не удобство, а необходимость: реактор мультиплексора не может замереть на
        // одном ребёнке, пока остальные панели ждут отрисовки.
        42 => {
            let (pid, nonblock) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1))
            };
            let mine = pid < t.procs.len() && t.procs[pid].parent == cur && t.procs[pid].zombie;
            let mut blocked = false;
            let result = if !mine {
                usize::MAX
            } else if let Some(code) = t.procs[pid].exit_code {
                // Ребёнок уже закончил: отдать код и ОТПУСТИТЬ слот — зомби больше не нужен.
                t.procs[pid].zombie = false;
                if t.procs[pid].state == State::Finished && pid != t.cur() {
                    t.free_slots.push(pid);
                }
                code
            } else if nonblock != 0 {
                WOULD_BLOCK
            } else {
                // Блокирующая форма переиспользует механизм `SYS_EXEC`: пробуждение и доставку
                // кода уже делает `wake_exec_waiters`, второго такого пути заводить незачем.
                t.procs[cur].state = State::ExecWait(pid);
                t.procs[pid].zombie = false; // код придёт напрямую, придерживать слот больше не надо
                blocked = true;
                0
            };
            if !blocked {
                let f = &mut t.procs[cur].frame;
                f.set_ret(result);
                f.advance();
            }
        }
        other => {
            let f = &mut t.procs[cur].frame;
            vprintln!("  [proc] неизвестный syscall {}", other);
            f.set_ret(usize::MAX);
            f.advance();
        }
    }
}

/// Доставить полезную нагрузку запроса: скопировать буфер отправителя `from` (`send_buf`/`send_len`)
/// в приёмный буфер получателя `to` (`recv_buf`/`recv_cap`), усекая по размеру приёмника.
/// Клиент в этот момент заблокирован — его память стабильна.
///
/// Веха 21.1: если отправитель передаёт capability (`send_cap` != MAX) — скопировать право в
/// домен получателя ([`cap::grant`], права как есть: аттенуация делается ЗАРАНЕЕ через
/// `CAP_DERIVE`) и зафиксировать c-space на диск ([`cap::persist`] — передача права = чекпойнт).
/// Возвращает (скопировано байт, дескриптор права у получателя | MAX).
pub(super) fn deliver_request(t: &Table, from: usize, to: usize) -> (usize, usize) {
    let mut n = t.procs[from].send_len.min(t.procs[to].recv_cap);
    // Веха 101 — усечение по приёмнику остаётся (менять семантику на живой системе дороже, чем
    // она стоит), но перестаёт быть НЕВИДИМЫМ: доставленную длину получает и лог, и сам
    // отправитель (третьим значением `SYS_CALL`, см. места вызова). До этого запрос молча
    // обрезался, а вызов рапортовал успех — так пропала половина сеянного `terminal.vv`.
    if n < t.procs[from].send_len {
        vprintln!(
            "  [ipc] P{} → P{}: запрос УСЕЧЁН {} → {} байт (буфер приёмника мал)",
            from, to, t.procs[from].send_len, n,
        );
    }
    // Веха 23: буферы обеих сторон могут лежать в ленивых кучах — доотобразить, иначе
    // постраничная трансляция молча пропустила бы немапленные страницы.
    if n > 0
        && !(ensure_heap_range(t, from, t.procs[from].send_buf, n)
            && ensure_heap_range(t, to, t.procs[to].recv_buf, n))
    {
        n = 0; // фреймы кончились — честнее не доставить ничего
    }
    if n > 0 {
        copy_between_spaces(
            arch::space_root(t.procs[from].space),
            t.procs[from].send_buf,
            arch::space_root(t.procs[to].space),
            t.procs[to].recv_buf,
            n,
        );
    }
    let mut tcap = usize::MAX;
    if t.procs[from].send_cap != usize::MAX {
        let c = Cap::from_bits(t.procs[from].send_cap as u64);
        // GRANT проверен при отправке (`CALL`); маска без сужения — копия прав как есть.
        if let Ok(nc) = cap::grant(t.procs[from].domain, c, t.procs[to].domain, Rights(u32::MAX)) {
            tcap = nc.bits() as usize;
            vprintln!(
                "  [cap] P{} → P{}: право [{}] передано в сообщении (grant по IPC)",
                from, to, cap::rights_str(cap::rights(t.procs[to].domain, nc).unwrap_or(Rights::NONE)),
            );
            cap::persist();
        }
    }
    (n, tcap)
}
