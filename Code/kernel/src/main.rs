//! Микроядро VOID — bare-metal: RISC-V (S-mode) и x86_64 (long mode).
//!
//! Веха 1: загрузка из OpenSBI, настройка стека, обнуление .bss, вывод в UART.
//! Веха 2: вектор trap'ов, обработка исключений (ebreak) и таймерные прерывания.
//! Веха 3: виртуальная память Sv39 (direct map RAM + W^X), включение трансляции.
//! Веха 4: куча ядра (free-list аллокатор + #[global_allocator]) → работает `alloc`.
//! Веха 5: кооперативная многозадачность (контексты + round-robin планировщик).
//! Веха 5.5: вытеснение по таймеру (preemption) — задачи чередуются без явного yield.
//! Веха 6: объектная модель — контент-адресуемые значения + изменяемые корни (стержень).
//! Веха 7.1: драйвер virtio-blk — чтение/запись секторов виртуального диска.
//! Веха 7.2: персистентный контент-адресуемый store — состояние переживает перезагрузку.
//! Веха 8: capability — непод­делываемые ссылки на объекты с правами (кто что может трогать).
//! Веха 9: async-executor — конкурентные future-задачи поверх объектного пространства.
//! Доводка (после Вехи 9): ленивая загрузка/мультикорни/coalescing, BLAKE3, GC+граф объектов,
//! virtio-blk на прерываниях (PLIC) + async I/O (пробуждение future из IRQ).
//! Веха 10: пользовательский режим (U-mode) + syscall'ы + процессы со своим адресным пространством.
//! Веха 19: программа как объект store — exec по content-id (Фаза 3, [[exec-from-store]]).
//! Веха 20: интерактивность — ввод UART по прерыванию, SYS_READ/SYS_EXEC, shell `vsh`.
//! Веха 21: capability по IPC (grant-в-сообщении, CAP_DERIVE) + персистентный c-space (.cspace).
//! Веха 22: куча процесса (SYS_MAP, ленивые страницы) + честные user page fault (гибнет процесс).
//! Веха 23: userspace целиком в ELF из store — секции `.user` больше нет; все программы сеются
//! в store под корни `bin/<имя>` и исполняются по content-id ([[elf-userspace]]).
//! Веха 24: граница архитектур — всё арх-специфичное за узким контрактом `arch/`
//! (riscv64 — рабочая реализация, x86_64 — заглушка); один код, два таргета cargo.
//! Веха 25: x86_64 оживает — PVH direct boot (трамплин 32→64), IDT + LAPIC-таймер,
//! 4-уровневый пейджинг W^X, переключение контекстов: ядерная половина демо работает
//! на обоих таргетах ([[x86-bringup]]).
//! Веха 26: userspace на x86_64 — ring3 (GDT/TSS) + `int 0x80` + iretq; программы
//! собираются под обе архитектуры, корни получили арх-измерение `bin/<arch>/<имя>`.
//! Веха 27: устройства x86_64 — virtio-blk-pci (поиск у арха: [[0005-multiarch-arch-boundary|
//! контракт]] `probe_virtio_blk` отдаёт транспорт mmio/pci общему драйверу), прерывание
//! диска MSI-X, консольный ввод IRQ4 через IOAPIC: персистентность на обеих архитектурах,
//! ОДИН диск несёт store с программами двух архитектур ([[x86-devices]]).
//! Веха 28: микробенчи (`bin/bench` + `bench_demo`, замер rdtime/rdtsc из U-mode) и отчёт
//! памяти — сравнение с Linux-гостем в том же QEMU (корневой README); на время замера
//! трассировка шлюзов глушится (`vprintln!`).
//! Веха 29: store вынесен в крейт `libs/void-store` за трейтом `BlockIo` — одна реализация
//! формата на ядро (virtio-blk) и хост ([[store-bridge]]): `void-store-import` кладёт
//! файлы и NAR-архивы (nix build → store) в образ диска без пересборки ядра.
//! Веха 30: контракт запуска процесса (ABI v2) — `SYS_EXEC` несёт argv, ребёнок наследует
//! env и стартовые capability (SYS_ARGS/SYS_STARTCAP, «preopen'ы»); персоналия выросла на
//! seek/rename; vsh: `run NAME ARGS`, `mv`, `tail` ([[process-contract]]) — фундамент std.
//! Веха 31: порт std ([[std-port]]) — таргеты `*-unknown-void` в форке rust (vendor/rust),
//! обычные Rust-программы обычным cargo; ядро подросло: стек процесса 64 КиБ, куча 16 МиБ
//! (std-ELF живёт в куче дважды: кэш store + копия загрузчика).
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod cap;
mod chan;
mod checkpoint;
mod elf;
mod executor;
mod frame;
mod heap;
mod linux;
mod object;
mod proc;
mod sched;
mod sync;
mod timer;
mod virtio_blk;
mod virtio_net;

/// Веха 23 — ELF-байты ВСЕХ userspace-программ, встроенные в образ ядра как СЕМЕНА. Собраны
/// `kernel/build.rs` отдельным `cargo build` крейта `programs/user` (свой target-dir в OUT_DIR)
/// под АРХИТЕКТУРУ ЭТОГО ядра. Это НЕ адреса исполнения — только сырые байты: [`seed_programs`]
/// кладёт их в объектный store под арх-корни `bin/<arch>/<имя>` (Веха 26; сев по хэшу:
/// изменился бинарь — корень атомарно переезжает), а исполняются программы всегда ОТТУДА,
/// по content-id ([`spawn_prog`], [[exec-from-store]]). Один диск может нести программы
/// нескольких архитектур — корни не пересекаются.
static PROGRAMS: &[(&str, &[u8])] = &[
    ("hello", include_bytes!(env!("PROG_HELLO"))),
    ("vsh", include_bytes!(env!("PROG_VSH"))),
    ("posixfs", include_bytes!(env!("PROG_POSIXFS"))),
    ("mini-sh", include_bytes!(env!("PROG_MINI_SH"))),
    ("blk-srv", include_bytes!(env!("PROG_BLK_SRV"))),
    ("blk-cli", include_bytes!(env!("PROG_BLK_CLI"))),
    ("obj-srv", include_bytes!(env!("PROG_OBJ_SRV"))),
    ("obj-cli", include_bytes!(env!("PROG_OBJ_CLI"))),
    ("cap-srv", include_bytes!(env!("PROG_CAP_SRV"))),
    ("cap-cli", include_bytes!(env!("PROG_CAP_CLI"))),
    ("busy", include_bytes!(env!("PROG_BUSY"))),
    ("heap", include_bytes!(env!("PROG_HEAP"))),
    ("crash", include_bytes!(env!("PROG_CRASH"))),
    ("bench", include_bytes!(env!("PROG_BENCH"))),
    ("net-srv", include_bytes!(env!("PROG_NET_SRV"))),
    ("threads", include_bytes!(env!("PROG_THREADS"))),
    ("freeze", include_bytes!(env!("PROG_FREEZE"))),
];

/// Арх-корень программы (Веха 26): `hello`/`bin/hello` → `bin/<arch>/<имя>`. Программы и
/// пользователь vsh говорят «bin/hello», не зная архитектуры; резолвит её ядро — так один
/// store (и один диск) несёт бинари нескольких архитектур бок о бок.
pub fn prog_root(name: &str) -> alloc::string::String {
    let short = name.strip_prefix("bin/").unwrap_or(name);
    alloc::format!("bin/{}/{}", arch::ARCH_NAME, short)
}

use core::fmt::Write;
use core::panic::PanicInfo;

/// Печать в UART0. Внутреннее API макросов `print!`/`println!`.
/// Выключаем прерывания на время строки: иначе вытеснение по таймеру могло бы
/// переключить задачу прямо посреди вывода, и строки разных задач перемешались бы.
pub fn _print(args: core::fmt::Arguments) {
    let sie = arch::irq_save_disable();
    let _ = arch::Console.write_fmt(args);
    arch::irq_restore(sie);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    ()              => { $crate::print!("\n") };
    ($($arg:tt)*)   => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

/// Главная функция ядра. Вызывается из `_start` с a0=hartid, a1=dtb.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: usize) -> ! {
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║  VOID — Веха 31                           ║");
    println!("  ║  порт std: обычный Rust на VOID           ║");
    println!("  ║  таргеты *-unknown-void · cargo build     ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();

    // Вектор trap'ов нужен и для page fault'ов, и для таймера.
    arch::trap_init();
    println!("  [trap] вектор установлен");

    // Инфраструктура из прошлых вех (кратко): trap-вектор, трансляция, куча.
    let root = arch::mm_init();
    // SAFETY: таблицы идентично отображают текущие PC/SP/UART.
    unsafe { arch::mm_enable(root) }
    println!("  [vm]   {} включён (direct map + W^X)", arch::MM_NAME);
    heap::init();
    println!("  [heap] куча ядра готова (16 МиБ)");
    println!();

    // Веха 7.1: подключить диск (нужен для персистентности).
    if virtio_blk::init() {
        println!("  [blk]  virtio-blk: {} секторов", virtio_blk::capacity_sectors());
    } else {
        println!("  [blk]  virtio-blk не найден — персистентность недоступна!");
    }

    // Веха 34: подключить сетевую карту (стек — в userspace net-srv, драйвер работает опросом).
    if virtio_net::init() {
        let m = virtio_net::mac();
        println!(
            "  [net]  virtio-net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5],
        );
    } else {
        println!("  [net]  virtio-net не найден — сеть недоступна");
    }

    // Прерывания устройств (Веха 24: одним вызовом контракта — контроллер, IRQ диска и
    // приём консоли по прерыванию). Байты консоли копятся в кольцевом буфере ядра с этого
    // момента — ввод, набранный (или поданный через pipe) во время демо, не теряется и
    // достанется shell'у в конце загрузки. Глобально прерывания включит timer::init;
    // синхронный путь на загрузке работает опросом.
    arch::init_device_interrupts();
    println!(
        "  [irq]  прерывания устройств вкл (диск IRQ {}, консоль IRQ {})",
        virtio_blk::irq(),
        arch::CONSOLE_IRQ,
    );

    // Веха 7.2: загрузить состояние с диска (RAM — кэш, диск — истина).
    if object::load() {
        println!(
            "  [store] загружено с диска: {} объектов, поколение {}",
            object::len(),
            object::generation(),
        );
        if let Some(rid) = object::root("system") {
            object::with(&rid, |b| {
                let text = b.and_then(|x| core::str::from_utf8(x).ok()).unwrap_or("?");
                println!("  [store] system root С ПРОШЛОГО запуска: \"{}\"", text);
            });
        }
    } else {
        println!("  [store] диск пуст — это ПЕРВЫЙ запуск");
    }

    // Веха 21.3: поднять персистентный c-space из спец-корня `.cspace` — домены со слотами,
    // поколениями и правами на долговечные цели (store/устройства/значения/корни). Дальше
    // create_domain переиспользует их по имени: процесс новой загрузки находит свои права.
    let ndom = cap::load();
    if ndom > 0 {
        println!("  [cap]  c-space восстановлен из .cspace: {} доменов", ndom);
    }

    // Веха 23: посеять/обновить программы системы в store — с этого момента ВЕСЬ userspace
    // живёт там и исполняется по content-id; в образе ядра остались только семена.
    seed_programs();
    println!();

    // Веха 8: capability поверх объектного store (c-space в RAM; персистентность — позже).
    cap_demo();
    println!();

    // Многозадачность: задачи пишут объекты в общий store (одинаковый вклад → дедуп).
    sched::init();
    sched::spawn("X", writer);
    sched::spawn("Y", writer);
    println!("  [sched] X/Y пишут объекты (вытесняются таймером):");
    timer::init();
    while sched::other_runnable() {
        arch::wait_for_interrupt(); // спать до следующего прерывания (таймера)
    }
    println!("  [sched] задачи завершились (вытеснений таймером: {})", timer::ticks());
    println!();

    // Веха 9: async-executor поверх объектного store (кооперативные future-задачи).
    async_demo();
    println!();

    // Веха 25/26: на архитектуре в bring-up (сегодня таких нет: riscv64 — с Вехи 10,
    // x86_64 — с Вехи 26) процессов ещё нет — ядерная половина системы уже прошла выше,
    // процессные демо и shell до готовности честно пропускаются.
    if !arch::USERSPACE_READY {
        println!("  [skip] демо процессов/exec/vsh: userspace этой архитектуры в bring-up");
    }

    // Веха 12: capability-защищённые IPC-эндпоинты. P0 = драйвер-сервер, ему ядро минтит cap на
    // УСТРОЙСТВО (право читать сектора). P1 = клиент, ему — cap на ЭНДПОИНТ сервера (право слать
    // ему сообщения). Без нужного cap ни IPC-вызов, ни доступ к диску невозможны — см. попытку
    // клиента прочитать диск напрямую в конце.
    if arch::USERSPACE_READY {
        proc_demo();
        println!();
    }

    // Веха 13/14: сервер объектного store в userspace. Процесс с cap на STORE отдаёт put/get и
    // set_root/get_root по IPC; клиент (лишь с cap на эндпоинт) кладёт значение, привязывает к
    // именованному корню и на СЛЕДУЮЩЕМ запуске читает прежнее значение обратно — persistence
    // через userspace-сервер, без прямого доступа к объектному пространству.
    if arch::USERSPACE_READY {
        store_demo();
        println!();
    }

    // Веха 21: передача capability по IPC + персистентный c-space. Клиент получает урезанное
    // право В ОТВЕТЕ сервера-раздатчика (первая загрузка) или находит его ВОССТАНОВЛЕННЫМ из
    // .cspace (последующие) — право переживает перезагрузку, тезис ADR 0002 полон.
    if arch::USERSPACE_READY {
        cap_ipc_demo();
        println!();
    }

    // Веха 16: вытеснение процессов. Два CPU-bound процесса БЕЗ единого yield/IPC — таймер
    // принудительно переключает их, и вывод меток перемежается (иначе один отработал бы до конца).
    if arch::USERSPACE_READY {
        preempt_demo();
        println!();
    }

    // Веха 22: куча процесса и честные фолты. Программа heap маппит 4 страницы лениво (0 фреймов
    // до первой записи), программа crash лезет по немапленному адресу — гибнет ОНА, а не ядро.
    if arch::USERSPACE_READY {
        mm_demo();
        println!();
    }

    // Веха 18.1: POSIX-персоналия как сервер. Процесс-программа пользуется только POSIX-подобными
    // open/write/close/read через IPC-shim; сервер-персоналия держит namespace файлов в своей RAM
    // (с Вехи 22.3 данные файлов — в его ленивой куче: 16 файлов × 4 КиБ вместо 4 × 256 байт).
    if arch::USERSPACE_READY {
        posix_demo();
        println!();
    }

    // Веха 19: «программа как объект store» — exec по content-id, а не по адресу в образе ядра.
    // Первый запуск сеет байты ELF (встроенные в ядро) в store под арх-корень bin/<arch>/hello;
    // второй — корень уже на диске, seed не участвует, ELF читается ИЗ STORE (kernel/src/elf.rs).
    if arch::USERSPACE_READY {
        exec_demo();
        println!();
    }

    // Веха 28: микробенчи — цена syscall/IPC/фолта/store/exec глазами userspace.
    if arch::USERSPACE_READY {
        bench_demo();
        println!();
    }

    // Доводка 3/4: структурные ссылки между объектами (граф) + версия дерева.
    gc_demo();
    println!();

    // Новый system root этого запуска.
    let marker = alloc::format!("boot #{} — VOID помнит своё состояние", object::generation() + 1);
    let id = object::put(marker.as_bytes());
    object::set_root("system", id);

    // GC: оставить только достижимое от корней. Веха 33 ([[commit-policy]]): жертвы
    // становятся НАДГРОБИЯМИ (кадры на диске не трогаются — учтённый мусор), а
    // уплотнение — не каждый boot, а ПО ПОРОГУ: когда мусора больше половины области.
    let (kept, collected) = object::gc();
    println!("  [gc] достижимо от корней: {}, собрано мусора: {}", kept, collected);
    let (garbage, area) = (object::garbage_bytes(), object::area_bytes());
    if garbage > 0 && garbage * 2 > area {
        println!(
            "  [gc] мусора {} КиБ из {} КиБ (>1/2) — уплотняем (двухфазно, крах-устойчиво)",
            garbage / 1024,
            area / 1024,
        );
        object::compact();
    } else {
        if garbage > 0 {
            println!(
                "  [gc] мусор копится: {} КиБ из {} КиБ (порог уплотнения — 1/2)",
                garbage / 1024,
                area / 1024,
            );
        }
        object::commit(); // точка синка загрузки: сев, system root, надгробия
    }
    println!("  [store] новый system root: \"{}\"", marker);
    println!(
        "  [store] commit → {} объектов, поколение {} · записано за сессию: {} КиБ",
        object::len(),
        object::generation(),
        object::bytes_written() / 1024,
    );
    println!();

    // Веха 20: интерактивная сессия — ФИНАЛЬНАЯ стадия вместо простоя. Система остаётся
    // живой, пока пользователь не наберёт `exit`. Записи файлов внутри сессии персистентны:
    // OBJ_SET_ROOT копится в пачку, фиксирует group commit (порог/период — Веха 33;
    // выключение питания в окно ≤ ~2 с теряет хвост, но store остаётся консистентным).
    if arch::USERSPACE_READY {
        shell_session();
        // Веха 33: конец сессии — точка жёсткого синка group commit: хвост
        // несинхронизированных операций (окно ≤ ~2 с) доезжает до диска.
        object::commit();
        println!(
            "  [store] финальный синк: поколение {} · записано за сессию: {} КиБ",
            object::generation(),
            object::bytes_written() / 1024,
        );
    }

    mem_report();
    println!();
    println!("  [idle] перезагрузи QEMU — состояние вернётся. Простаиваем до прерываний.");

    loop {
        arch::wait_for_interrupt();
    }
}

/// Веха 28: сколько RAM занимает система к концу загрузки. Образ ядра — от кода до
/// `_kernel_end` (включая .bss с семенами программ); дальше — всё, что раздал
/// bump-аллокатор фреймов (таблицы страниц, кольца virtio, арена кучи, страницы
/// процессов; освобождения нет — это ПИК, верхняя оценка).
fn mem_report() {
    extern "C" {
        static _text_start: u8;
        static _kernel_end: u8;
    }
    let image = &raw const _kernel_end as usize - &raw const _text_start as usize;
    println!(
        "  [mem] образ ядра: {} КиБ · фреймы после образа (пик): {} КиБ · RAM машины: 128 МиБ",
        image / 1024,
        frame::used_bytes() / 1024,
    );
}

/// Веха 23: посеять/обновить программы системы в store. Каждая — объект под арх-корнем
/// `bin/<arch>/<имя>` (Веха 26, [`prog_root`]); сев по хэшу: content-id совпал — корень
/// актуален (диск — истина, семя не участвует); разошёлся — корень атомарно переезжает на
/// новую версию (обновление системы = смена корня, старые байты уходят в GC как мусор).
/// Дедуп store делает повторный сев бесплатным. Доарховые корни `bin/<имя>` (Вехи 23–25)
/// мигрируются: снимаем их, чтобы старые ELF не жили вечно якорями GC.
fn seed_programs() {
    let (mut fresh, mut sown, mut updated, mut migrated) = (0usize, 0usize, 0usize, 0usize);
    for (name, bytes) in PROGRAMS {
        let root_name = prog_root(name);
        let id = object::put(bytes);
        match object::root(&root_name) {
            Some(old) if old == id => fresh += 1,
            Some(_) => {
                object::set_root(&root_name, id);
                updated += 1;
            }
            None => {
                object::set_root(&root_name, id);
                sown += 1;
            }
        }
        if object::del_root(&alloc::format!("bin/{}", name)) {
            migrated += 1; // legacy-корень снят — байты уйдут ближайшим GC
        }
    }
    println!(
        "  [seed] программы в store ({} корней bin/{}/*): {} актуально, {} посеяно, {} обновлено",
        PROGRAMS.len(),
        arch::ARCH_NAME,
        fresh,
        sown,
        updated,
    );
    if migrated > 0 {
        println!("  [seed] мигрировано со старых корней bin/*: {}", migrated);
    }
}

/// Запустить программу из store по имени (Веха 23): арх-корень ([`prog_root`]) → content-id →
/// байты ELF → [`proc::spawn_elf`]. ВСЕ процессы системы приходят только этим путём — тем же,
/// каким `SYS_EXEC` запускает программы для vsh. `pname` — имя процесса/домена (личность в .cspace).
fn spawn_prog(name: &str, pname: &'static str, arg: usize) -> usize {
    let root_name = prog_root(name);
    let id = object::root(&root_name)
        .unwrap_or_else(|| panic!("{} не посеян в store", root_name));
    let bytes = object::with(&id, |b| b.map(|x| x.to_vec()))
        .unwrap_or_else(|| panic!("объект корня {} недоступен", root_name));
    match proc::spawn_elf(pname, &bytes, arg) {
        Ok(pid) => pid,
        Err(e) => panic!("негодный ELF под корнем {}: {:?}", root_name, e),
    }
}

/// Веха 22: куча процесса + честные фолты. `heap` просит 4 страницы через SYS_MAP — ядро НЕ
/// выделяет ни одного фрейма (ленивый резерв); каждая первая запись в страницу даёт page fault,
/// по которому ядро выделяет обнулённый фрейм и повторяет инструкцию. `crash` обращается по
/// немапленному адресу вне кучи — ядро убивает ЕГО (родителю в SYS_EXEC ушёл бы MAX), а само
/// живёт дальше: раньше такой фолт валил всю систему «неожиданным trap'ом».
fn mm_demo() {
    println!("  [mm] куча процесса (SYS_MAP, ленивые страницы) + честные фолты (Веха 22):");
    spawn_prog("heap", "heap", 0);
    spawn_prog("crash", "crash", 0);
    proc::run();
    println!("  [mm] сессия памяти завершена: crash убит, ядро и остальные живы");
}

/// Веха 21: cap-transfer по IPC + персистентный c-space. Сервер-раздатчик держит cap на store
/// `[rw-g-]`; клиент — только эндпоинт. Первая загрузка: клиент просит доступ и получает В
/// ОТВЕТЕ урезанную копию `[r-g--]` (CAP_DERIVE + grant-в-сообщении); передача = чекпойнт
/// c-space в `.cspace`. Следующие загрузки: ядро находит право в ВОССТАНОВЛЕННОМ домене
/// клиента и отдаёт его дескриптор без повторной выдачи — capability пережила перезагрузку.
fn cap_ipc_demo() {
    use void_abi::Rights;

    println!("  [cap] передача права по IPC + персистентный c-space (Веха 21):");
    let server = spawn_prog("cap-srv", "cap-srv", 0);
    let rwg = Rights::READ.union(Rights::WRITE).union(Rights::GRANT);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, rwg);
    proc::set_arg(server, scap.bits() as usize);
    let client = spawn_prog("cap-cli", "cap-cli", 0);
    let ep = cap::mint(proc::domain(client), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(client, ep.bits() as usize);
    println!(
        "    P{} 'cap-srv' [store {}] ← P{} 'cap-cli' [эндпоинт {}]",
        server, cap::rights_str(rwg), client, cap::rights_str(Rights::SEND),
    );
    // Право с прошлой загрузки? Домен клиента поднят из .cspace вместе со слотами — если там
    // выжил cap на store, отдаём его дескриптор процессу (сам он дескрипторов не помнит:
    // процессы пока эфемерны, персистентна их ЛИЧНОСТЬ — домен по имени).
    match cap::find_store_cap(proc::domain(client)) {
        Some((c, r)) => {
            println!(
                "    у 'cap-cli' УЖЕ есть store-cap [{}] — восстановлен из .cspace, минт не нужен",
                cap::rights_str(r),
            );
            proc::set_arg2(client, c.bits() as usize);
        }
        None => {
            println!("    прав на store у 'cap-cli' нет — попросит у раздатчика по IPC");
            proc::set_arg2(client, usize::MAX);
        }
    }
    proc::run();
    println!("  [cap] сессия раздатчика завершена — обратно в ядро");
}

/// Веха 20.4: интерактивная сессия. Сервер-персоналия получает cap на store (r/w — файлы), а
/// `vsh` — ДВА начальных cap: эндпоинт персоналии (a0, право SEND) и cap на store ТОЛЬКО с
/// правом EXEC (a1): запускать программы можно, читать/писать объекты напрямую — нельзя
/// (аттенуация «только запуск»). Ввод — SYS_READ с UART по прерыванию; `run bin/hello`
/// исполняет ELF из store по имени корня (машинерия Вехи 19 руками пользователя).
fn shell_session() {
    use void_abi::Rights;

    println!("  [vsh] интерактивная сессия (Веха 20) — ls · cat · echo · run bin/hello · ping · exit:");
    let server = spawn_prog("posixfs", "posixfs", 0);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, Rights::READ.union(Rights::WRITE));
    proc::set_arg(server, scap.bits() as usize);

    // Веха 34: сетевой сервер — ему cap на сетевое устройство (r/w — слать/принимать кадры).
    // vsh получит эндпоинт на него (start-cap слот 2) и команду `ping`.
    let netsrv = spawn_prog("net-srv", "net-srv", 0);
    let netdev = cap::mint(
        proc::domain(netsrv),
        cap::Target::Device(cap::Device::Net),
        Rights::READ.union(Rights::WRITE),
    );
    proc::set_arg(netsrv, netdev.bits() as usize);

    let sh = spawn_prog("vsh", "vsh", 0);
    let ep = cap::mint(proc::domain(sh), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(sh, ep.bits() as usize);
    // Веха 37: к EXEC добавился WRITE — SYS_CHECKPOINT пишет образ процесса в store,
    // а право наследуют дети vsh (именно ОНИ себя морозят). Аттенуация никуда не делась:
    // передать дальше урезанную копию можно cap_derive'ом.
    let xcap = cap::mint(proc::domain(sh), cap::Target::Store, Rights::EXEC.union(Rights::WRITE));
    proc::set_arg2(sh, xcap.bits() as usize);
    let netep = cap::mint(proc::domain(sh), cap::Target::Endpoint(netsrv), Rights::SEND);
    // Веха 30 — контракт запуска: те же права — в таблицу стартовых capability
    // (её унаследуют программы, которые vsh запустит через SYS_EXEC), плюс окружение.
    // Слот 0 — эндпоинт персоналии, 1 — EXEC, 2 — эндпоинт net-srv (Веха 34).
    proc::push_start_cap(sh, ep.bits() as usize);
    proc::push_start_cap(sh, xcap.bits() as usize);
    proc::push_start_cap(sh, netep.bits() as usize);
    proc::set_env(sh, alloc::format!("ARCH={}\0SYSTEM=void\0", arch::ARCH_NAME).as_bytes());
    println!(
        "    P{} 'posixfs' [{}] ← P{} 'vsh' [эндпоинт {} + store {}]",
        server,
        cap::rights_str(Rights::READ.union(Rights::WRITE)),
        sh,
        cap::rights_str(Rights::SEND),
        cap::rights_str(Rights::EXEC),
    );
    proc::run();
    println!("  [vsh] сессия завершена (exit) — обратно в ядро");
}

/// Веха 8: демонстрация свойств capability на общем объектном store.
/// Два домена: `kernel` владеет объектами, `app` получает только то, что ему передали.
fn cap_demo() {
    use void_abi::{Cap, Rights};

    println!("  [cap] capability — неподделываемость · аттенуация · отзыв:");

    let kernel = cap::create_domain("kernel");
    let app = cap::create_domain("app");

    // Неизменяемое значение + изменяемая ячейка-корень на него.
    let v0 = object::put(b"demo: version 0");
    object::set_root("demo-cell", v0);

    // 1) Владелец минтит ПОЛНЫЙ capability на ячейку (r/w/g).
    let owner = cap::mint(kernel, cap::Target::Root("demo-cell"), Rights::ALL);
    println!("    {} минтит cap->'demo-cell' [{}]", cap::domain_name(kernel), cap::rights_str(Rights::ALL));

    // 2) Передаёт в 'app' СУЖЕННУЮ копию — только чтение (аттенуация: rwg ∩ r-- = r--).
    let ro = cap::grant(kernel, owner, app, Rights::READ).unwrap();
    let got = cap::rights(app, ro).unwrap();
    println!("    grant -> {} с маской [r--] → фактически [{}]  (расширить нельзя)",
        cap::domain_name(app), cap::rights_str(got));

    // 3) app читает своим cap — разрешено.
    let _ = cap::read(app, ro, |b| {
        println!("    {} читает ячейку: \"{}\"", cap::domain_name(app), core::str::from_utf8(b).unwrap_or("?"));
    });

    // 4) app пытается ПИСАТЬ своим read-only cap — отказ (нет WRITE).
    let v1 = object::put(b"demo: version 1 (owner-written)");
    match cap::write_root(app, ro, v1) {
        Ok(()) => println!("    app записал?! — БАГ"),
        Err(e) => println!("    app write отклонён: {:?}  ← нет права WRITE", e),
    }

    // 5) app пытается ПЕРЕДАТЬ дальше — отказ (нет GRANT, аттенуация сработала).
    let other = cap::create_domain("other");
    match cap::grant(app, ro, other, Rights::READ) {
        Ok(_) => println!("    app передал дальше?! — БАГ"),
        Err(e) => println!("    app grant отклонён: {:?}  ← нет права GRANT", e),
    }

    // 6) Владелец ПИШЕТ своим полным cap — разрешено (ячейка переезжает на v1).
    cap::write_root(kernel, owner, v1).unwrap();
    let _ = cap::read(kernel, owner, |b| {
        println!("    {} пишет v1, читает: \"{}\"", cap::domain_name(kernel), core::str::from_utf8(b).unwrap_or("?"));
    });

    // 7) Capability и на НЕИЗМЕНЯЕМОЕ значение (не только на ячейку): read-only по природе.
    let vcap = cap::mint(app, cap::Target::Value(v0), Rights::READ);
    let _ = cap::read(app, vcap, |b| {
        println!("    cap на значение v0: \"{}\"  (значения неизменяемы → только чтение)",
            core::str::from_utf8(b).unwrap_or("?"));
    });

    // 8) Подделка: 'app' предъявляет выдуманный дескриптор — отвергнут таблицей.
    let forged = Cap::new(999, 1);
    match cap::read(app, forged, |_| {}) {
        Ok(()) => println!("    подделка сработала?! — БАГ"),
        Err(e) => println!("    подделанный cap отвергнут: {:?}  ← неподделываемость", e),
    }

    // 9) Отзыв: kernel отзывает свой cap; прежний дескриптор устаревает (поколение++).
    cap::revoke(kernel, owner).unwrap();
    match cap::read(kernel, owner, |_| {}) {
        Ok(()) => println!("    доступ после отзыва?! — БАГ"),
        Err(e) => println!("    cap после отзыва: {:?}  ← revocation", e),
    }
}

/// Веха 9: демонстрация async-executor'а поверх объектного store.
/// Два производителя кладут значения и шлют их адреса через async-канал; потребитель
/// принимает и читает из store. Всё — на одном стеке, кооперативно через `.await`.
fn async_demo() {
    println!("  [async] executor — future-задачи поверх объектного store:");

    let (tx, rx) = chan::channel::<void_abi::ContentId>();

    // Async I/O: прочитать сектор 0 БЕЗ опроса — future паркуется до прерывания диска (IRQ).
    executor::spawn(async {
        match virtio_blk::read_async(0).await {
            Some(sec) => println!(
                "    [D] async-чтение сектора 0: [{:02x} {:02x} {:02x} {:02x}] — разбужен IRQ диска",
                sec[0], sec[1], sec[2], sec[3],
            ),
            None => println!("    [D] async-чтение сектора 0 не удалось"),
        }
    });

    // Потребитель: печатает объекты по мере поступления их адресов. Паркуется на пустом
    // канале (Pending) и просыпается, когда производитель пришлёт (send → waker).
    executor::spawn(async move {
        for _ in 0..6 {
            let id = rx.recv().await;
            object::with(&id, |b| {
                let s = b.and_then(|x| core::str::from_utf8(x).ok()).unwrap_or("?");
                println!("    [C] принял {} = \"{}\"", id_short(&id), s);
            });
        }
        println!("    [C] всё принято — завершаюсь");
    });

    // Два производителя: кладут значение в store, шлют адрес, уступают через yield.await.
    for who in ["A", "B"] {
        let tx = tx.clone();
        executor::spawn(async move {
            for i in 0..3 {
                let data = alloc::format!("async-{}-{}", who, i);
                let id = object::put(data.as_bytes());
                println!("    [{}] put \"{}\"", who, data);
                tx.send(id);
                executor::yield_now().await; // дать другим задачам продвинуться
            }
        });
    }
    drop(tx); // исходный отправитель больше не нужен (копии — у производителей)

    executor::run(); // крутить, пока все задачи не завершатся
    println!("    [async] прерываний диска обработано: {}", virtio_blk::irq_count());
}

/// Веха 12: capability-защищённые IPC-эндпоинты. Драйвер-сервер получает cap на УСТРОЙСТВО,
/// клиент — cap на ЭНДПОИНТ сервера. `SYS_CALL`/`SYS_BLK_READ` берут дескриптор, а не сырой
/// pid/номер: без нужного права ядро отказывает. Соединяет [[capabilities]] с [[processes]].
fn proc_demo() {
    use void_abi::Rights;

    println!("  [proc] userspace-драйвер + IPC под защитой capability:");

    // Сервер-драйвер: ядро минтит ему cap на УСТРОЙСТВО (право читать сектора). Дескриптор
    // передаём процессу через a0 — c-space внутри U-mode недоступен, cap живёт как число.
    let server = spawn_prog("blk-srv", "blk-drv", 0);
    let devr = Rights::READ.union(Rights::WRITE); // Веха 17: драйвер умеет и читать, и писать
    let dev = cap::mint(proc::domain(server), cap::Target::Device(cap::Device::Block), devr);
    proc::set_arg(server, dev.bits() as usize);
    println!(
        "    P{} '{}' ← cap на устройство [{}]",
        server, cap::domain_name(proc::domain(server)), cap::rights_str(devr),
    );

    // Клиент: cap на ЭНДПОИНТ сервера (право слать ему сообщения). Cap на устройство он НЕ
    // получает — поэтому прямой BLK_READ у него в конце отвергается.
    let client = spawn_prog("blk-cli", "blk-cli", 0);
    let ep = cap::mint(proc::domain(client), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(client, ep.bits() as usize);
    println!(
        "    P{} '{}' ← cap на эндпоинт P{} [{}]",
        client, cap::domain_name(proc::domain(client)), server, cap::rights_str(Rights::SEND),
    );

    proc::run();
    println!("  [proc] сессия процессов завершена — обратно в ядро");
}

/// Веха 13: сервер объектного store в userspace. Сервер держит cap на STORE (r/w) и обслуживает
/// put/get по IPC; клиент с cap лишь на ЭНДПОИНТ кладёт значение, получает его content-id и
/// читает обратно. Так персистентное [[object-model|пространство]] отдаётся как сервис под
/// capability, а не зашивается в каждый процесс ([[0002-...|ADR 0002]]).
fn store_demo() {
    use void_abi::Rights;

    println!("  [proc] userspace-сервер объектного store + клиент через IPC:");

    // Сервер store: cap на сам store с правами читать и писать (r-w-).
    let rw = Rights::READ.union(Rights::WRITE);
    let server = spawn_prog("obj-srv", "obj-store", 0);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, rw);
    proc::set_arg(server, scap.bits() as usize);
    println!(
        "    P{} '{}' ← cap на store [{}]",
        server, cap::domain_name(proc::domain(server)), cap::rights_str(rw),
    );

    // Клиент: cap только на ЭНДПОИНТ сервера. Прямого доступа к store у него нет.
    let client = spawn_prog("obj-cli", "store-cli", 0);
    let ep = cap::mint(proc::domain(client), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(client, ep.bits() as usize);
    println!(
        "    P{} '{}' ← cap на эндпоинт P{} [{}]",
        client, cap::domain_name(proc::domain(client)), server, cap::rights_str(Rights::SEND),
    );

    proc::run();
    println!("  [proc] сессия store завершена — обратно в ядро");
}

/// Веха 16: вытеснение процессов по таймеру. Два CPU-bound процесса крутят busy-loop без единого
/// `yield`/IPC и печатают свою метку. Кооперативно один отработал бы все печати до второго; с
/// вытеснением таймер принудительно переключает их — метки ` A `/` B ` перемежаются.
fn preempt_demo() {
    println!("  [proc] вытеснение по таймеру — два CPU-bound процесса без yield:");
    print!("    ");
    // a0 выбирает метку внутри программы (0 = " A ", 1 = " B ") — обе печати из её .rodata.
    let a = spawn_prog("busy", "busy-A", 0);
    proc::set_arg(a, 0);
    let b = spawn_prog("busy", "busy-B", 0);
    proc::set_arg(b, 1);
    let before = timer::ticks();
    proc::run();
    println!();
    println!(
        "  [proc] оба процесса завершились; вытеснений таймером за сессию: {}",
        timer::ticks() - before,
    );
}

/// Веха 18.1–18.4: POSIX-персоналия как сервер. Клиент `mini-sh` (Веха 18.4) написан ЦЕЛИКОМ на
/// POSIX-shim (libc-заглушка): ни `ecall`, ни op-кодов, ни capability в теле — программа «не знает»
/// ни про IPC, ни про VOID, только про `open/read/write/close/readdir`. Играет сессию
/// `cat; echo > ; cat; ls` над `motd.txt`; файл персистится через store (Вехи 18.2/18.3).
fn posix_demo() {
    use void_abi::Rights;

    println!("  [proc] POSIX-персоналия + программа mini-sh на чистом POSIX-shim (Веха 18.4):");
    // Персоналии — cap на store (файлы персистятся под корнями-именами, Веха 18.2).
    let server = spawn_prog("posixfs", "posixfs", 0);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, Rights::READ.union(Rights::WRITE));
    proc::set_arg(server, scap.bits() as usize);
    let client = spawn_prog("mini-sh", "mini-sh", 0);
    let ep = cap::mint(proc::domain(client), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(client, ep.bits() as usize);
    println!(
        "    P{} '{}' [cap store] ← клиент P{} '{}' [cap эндпоинт]",
        server, cap::domain_name(proc::domain(server)), client, cap::domain_name(proc::domain(client)),
    );
    proc::run();
    println!("  [proc] сессия персоналии завершена — обратно в ядро");
}

/// Веха 19 — «программа как объект store»: exec по content-id, показанный ЯВНО (с печатью
/// content-id и длины). С Вехи 23 этим путём приходят ВСЕ процессы ([`spawn_prog`]); здесь он
/// разобран по шагам на `bin/hello`: корень (посеян [`seed_programs`], на втором запуске — уже
/// с диска) → content-id → байты ИЗ STORE → [`proc::spawn_elf`] ([[exec-from-store]]). Один и
/// тот же бинарь даёт один и тот же content-id на обеих загрузках — контент-адресация видима.
fn exec_demo() {
    println!("  [exec] программа как объект store — exec по content-id (Веха 19):");

    let id = object::root(&prog_root("hello")).expect("bin/<arch>/hello посеян при загрузке");

    // Прочитать байты ИЗ STORE по content-id (а не байты-семя из образа ядра!) — копируем в
    // Vec и сразу отпускаем замок store: дальше `proc::run()` крутит процесс до завершения.
    let elf_bytes = object::with(&id, |b| b.map(|bytes| bytes.to_vec()));
    match elf_bytes {
        Some(bytes) => {
            println!(
                "    прочитано {} байт ELF из store по content-id {} — грузим",
                bytes.len(), id_short(&id),
            );
            match proc::spawn_elf("hello", &bytes, 0) {
                Ok(pid) => {
                    println!("    P{} 'hello' запущен — вход по e_entry из ELF, не по адресу в ядре", pid);
                    proc::run();
                    println!("    [exec] сессия hello завершена — обратно в ядро");
                }
                Err(e) => println!("    [exec] ELF-загрузчик отказал: {:?}  ← exec не выполнен", e),
            }
        }
        None => println!("    [exec] не удалось прочитать ELF из store по content-id (не должно случаться)"),
    }
}

/// Веха 28: микробенчи из userspace — программа `bench` с capability на store [r w x]
/// и эндпоинт posixfs меряет rdtime/rdtsc'ом цену null-syscall'а, IPC-круга, page
/// fault'а, obj_put/obj_get и полного exec. Трассировка шлюзов ([ipc]/[obj]/[mm]…)
/// на время сессии глушится — иначе мерился бы println, а не системный путь.
/// Цифры — ЭМУЛЯЦИЯ (QEMU TCG): сравнивать честно только с гостём в том же QEMU.
fn bench_demo() {
    use void_abi::Rights;

    println!("  [bench] микробенчи (QEMU TCG — цифры эмуляции, не железа):");
    let server = spawn_prog("posixfs", "posixfs", 0);
    let srv_cap =
        cap::mint(proc::domain(server), cap::Target::Store, Rights::READ.union(Rights::WRITE));
    proc::set_arg(server, srv_cap.bits() as usize);

    let bench = spawn_prog("bench", "bench", 0);
    let rwx = Rights::READ.union(Rights::WRITE).union(Rights::EXEC);
    let scap = cap::mint(proc::domain(bench), cap::Target::Store, rwx);
    proc::set_arg(bench, scap.bits() as usize);
    let ep = cap::mint(proc::domain(bench), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg2(bench, ep.bits() as usize);

    proc::set_verbose(false);
    proc::run();
    proc::set_verbose(true);
    println!("  [bench] сессия завершена");
}

/// Доводка: структурные ссылки между объектами + смена версии (готовит мусор для GC).
/// Узел ссылается на листья по их ContentId — контент-адресуемый граф (как деревья git).
fn gc_demo() {
    println!("  [gc] структурные ссылки — узел ссылается на листья по ContentId:");
    let a = object::put(b"leaf-A");
    let b = object::put(b"leaf-B");
    let c = object::put(b"leaf-C");

    // Версия 1 дерева ссылается на A и B.
    let v1 = object::put_node(b"tree v1", &[a, b]);
    object::set_root("tree", v1);
    println!("    tree v1 -> [A, B]  корень 'tree' = {}", id_short(&v1));

    // Версия 2 ссылается на B и C; корень переезжает → A и v1 осиротели.
    let v2 = object::put_node(b"tree v2", &[b, c]);
    object::set_root("tree", v2);
    println!(
        "    tree v2 -> [{} детей]  корень 'tree' = {}  (leaf-A и v1 теперь недостижимы)",
        object::children(&v2).len(),
        id_short(&v2),
    );
}

/// Задача-писатель: кладёт несколько значений в общий объектный store.
fn writer() {
    let name = sched::current_name();
    for i in 0..3 {
        let data = alloc::format!("{}-value-{}", name, i);
        let id = object::put(data.as_bytes());
        println!("    [{}] put \"{}\" -> {}", name, data, id_short(&id));
        // Немного занятости, чтобы дать таймеру шанс вытеснить между записями. `black_box` —
        // чтобы оптимизатор (ядро с Вехи 23 собирается с opt-level>0) не выкинул цикл.
        let mut acc = 0u64;
        for k in 0..250_000u64 {
            acc = acc.wrapping_add(core::hint::black_box(k));
        }
        core::hint::black_box(acc);
    }
}

/// Короткое hex-представление контент-адреса (первые 5 байт) — для вывода.
fn id_short(id: &void_abi::ContentId) -> alloc::string::String {
    use core::fmt::Write as _;
    let mut s = alloc::string::String::new();
    for byte in &id.0[..5] {
        let _ = write!(s, "{:02x}", byte);
    }
    s.push('…');
    s
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("  [PANIC] {}", info);
    loop {
        arch::wait_for_interrupt();
    }
}
