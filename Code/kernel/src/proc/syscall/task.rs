//! Веха 214.6 — системные вызовы: **процессы, нити и сеанс**.
//!
//! Завести, подождать, снять, уступить ход; нити и их сон на futex; чекпойнт и разморозка;
//! аргументы, окружение и стартовые права.
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
    match num {
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
                                    return true;
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
                return true;
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
        _ => return false,
    }
    true
}
