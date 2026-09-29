//! Веха 214.6 — системные вызовы: **устройства и ввод-вывод**.
//!
//! Консоль, сеть, мышь, клавиатура, экран, журнал ядра, конфигурация PCI и чтение регистров.
//! Всё, что упирается в железо, — но не драйверы: те живут процессами.
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
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
                return true;
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
        _ => return false,
    }
    true
}
