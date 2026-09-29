//! Веха 214.6 — системные вызовы: **наблюдение за системой**.
//!
//! Время, случайность, подробность журнала, сон, выключение и обзор процессов: список, права,
//! состояние, отзыв. Право `sysview` — единственное, чем это открывается.
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
    match num {
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
                return true;
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
                return true;
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
        _ => return false,
    }
    true
}
