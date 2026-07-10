//! Микроядро VOID — bare-metal RISC-V, S-mode.
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
//! См. роадмап и ADR в Obsidian (`10-projects/void/`).
#![no_std]
#![no_main]

extern crate alloc;

mod cap;
mod chan;
mod context;
mod csr;
mod executor;
mod frame;
mod heap;
mod object;
mod paging;
mod plic;
mod proc;
mod sbi;
mod sched;
mod sync;
mod timer;
mod trap;
mod uart;
mod user;
mod virtio_blk;

use core::fmt::Write;
use core::panic::PanicInfo;

// Ассемблерная точка входа `_start` (см. entry.s) → вызывает `kmain`.
core::arch::global_asm!(include_str!("entry.s"));

/// Печать в UART0. Внутреннее API макросов `print!`/`println!`.
/// Выключаем прерывания на время строки: иначе вытеснение по таймеру могло бы
/// переключить задачу прямо посреди вывода, и строки разных задач перемешались бы.
pub fn _print(args: core::fmt::Arguments) {
    let sie = csr::irq_save_disable();
    let _ = uart::Uart.write_fmt(args);
    csr::irq_restore(sie);
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
    println!("  ║  VOID — Веха 9                            ║");
    println!("  ║  async-executor · future над объектами    ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  hart id : {}", hartid);
    println!("  dtb     : {:#x}", dtb);
    println!("  void-abi: v{}", void_abi::VERSION);
    println!();

    // Вектор trap'ов нужен и для page fault'ов, и для таймера.
    trap::init();
    println!("  [trap] вектор установлен");

    // Инфраструктура из прошлых вех (кратко): trap-вектор, Sv39, куча.
    let root = paging::init();
    // SAFETY: таблицы идентично отображают текущие PC/SP/UART.
    unsafe { paging::enable(root) }
    println!("  [vm]   Sv39 включён (direct map + W^X)");
    heap::init();
    println!("  [heap] куча ядра готова (2 МиБ)");
    println!();

    // Веха 7.1: подключить диск (нужен для персистентности).
    if virtio_blk::init() {
        println!("  [blk]  virtio-blk: {} секторов", virtio_blk::capacity_sectors());
    } else {
        println!("  [blk]  virtio-blk не найден — персистентность недоступна!");
    }

    // Прерывания устройств: настроить PLIC на IRQ диска и разрешить внешние прерывания S-mode.
    // Глобально прерывания включит timer::init; синхронный путь на загрузке работает опросом.
    if virtio_blk::irq() != 0 {
        plic::init(virtio_blk::irq());
        csr::enable_external_interrupt();
        println!("  [plic] внешние прерывания вкл (virtio-blk IRQ {})", virtio_blk::irq());
    }

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
        // SAFETY: wfi — спать до следующего прерывания (таймера).
        unsafe { core::arch::asm!("wfi") }
    }
    println!("  [sched] задачи завершились (вытеснений таймером: {})", timer::ticks());
    println!();

    // Веха 9: async-executor поверх объектного store (кооперативные future-задачи).
    async_demo();
    println!();

    // Веха 12: capability-защищённые IPC-эндпоинты. P0 = драйвер-сервер, ему ядро минтит cap на
    // УСТРОЙСТВО (право читать сектора). P1 = клиент, ему — cap на ЭНДПОИНТ сервера (право слать
    // ему сообщения). Без нужного cap ни IPC-вызов, ни доступ к диску невозможны — см. попытку
    // клиента прочитать диск напрямую в конце.
    proc_demo();
    println!();

    // Веха 13/14: сервер объектного store в userspace. Процесс с cap на STORE отдаёт put/get и
    // set_root/get_root по IPC; клиент (лишь с cap на эндпоинт) кладёт значение, привязывает к
    // именованному корню и на СЛЕДУЮЩЕМ запуске читает прежнее значение обратно — persistence
    // через userspace-сервер, без прямого доступа к объектному пространству.
    store_demo();
    println!();

    // Веха 16: вытеснение процессов. Два CPU-bound процесса БЕЗ единого yield/IPC — таймер
    // принудительно переключает их, и вывод меток перемежается (иначе один отработал бы до конца).
    preempt_demo();
    println!();

    // Веха 18.1: POSIX-персоналия как сервер. Процесс-программа пользуется только POSIX-подобными
    // open/write/close/read через IPC-shim; сервер-персоналия держит namespace файлов в своей RAM.
    posix_demo();
    println!();

    // Доводка 3/4: структурные ссылки между объектами (граф) + версия дерева.
    gc_demo();
    println!();

    // Новый system root этого запуска.
    let marker = alloc::format!("boot #{} — VOID помнит своё состояние", object::generation() + 1);
    let id = object::put(marker.as_bytes());
    object::set_root("system", id);

    // GC: оставить только достижимое от корней (system, demo-cell, tree); затем commit
    // уплотняет диск. Осиротевшие put'ы (X/Y/async, старые версии) — это мусор, их соберём.
    let (kept, collected) = object::gc();
    println!("  [gc] достижимо от корней: {}, собрано мусора: {}", kept, collected);
    object::commit();
    println!("  [store] новый system root: \"{}\"", marker);
    println!(
        "  [store] commit → {} объектов, поколение {} (уплотнено на диск)",
        object::len(),
        object::generation(),
    );
    println!();
    println!("  [idle] перезагрузи QEMU — состояние вернётся. Простаиваем (wfi).");

    loop {
        // SAFETY: wfi — ждать прерывания; в S-mode разрешено.
        unsafe { core::arch::asm!("wfi") }
    }
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
    let server = proc::spawn("blk-drv", user::blk_server_entry(), 0);
    let devr = Rights::READ.union(Rights::WRITE); // Веха 17: драйвер умеет и читать, и писать
    let dev = cap::mint(proc::domain(server), cap::Target::Device(cap::Device::Block), devr);
    proc::set_arg(server, dev.bits() as usize);
    println!(
        "    P{} '{}' ← cap на устройство [{}]",
        server, cap::domain_name(proc::domain(server)), cap::rights_str(devr),
    );

    // Клиент: cap на ЭНДПОИНТ сервера (право слать ему сообщения). Cap на устройство он НЕ
    // получает — поэтому прямой BLK_READ у него в конце отвергается.
    let client = proc::spawn("blk-cli", user::blk_client_entry(), 0);
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
    let server = proc::spawn("obj-store", user::store_server_entry(), 0);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, rw);
    proc::set_arg(server, scap.bits() as usize);
    println!(
        "    P{} '{}' ← cap на store [{}]",
        server, cap::domain_name(proc::domain(server)), cap::rights_str(rw),
    );

    // Клиент: cap только на ЭНДПОИНТ сервера. Прямого доступа к store у него нет.
    let client = proc::spawn("store-cli", user::store_client_entry(), 0);
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
    let a = proc::spawn("busy-A", user::busy_entry(), 0);
    proc::set_arg(a, user::label_a());
    let b = proc::spawn("busy-B", user::busy_entry(), 0);
    proc::set_arg(b, user::label_b());
    let before = timer::ticks();
    proc::run();
    println!();
    println!(
        "  [proc] оба процесса завершились; вытеснений таймером за сессию: {}",
        timer::ticks() - before,
    );
}

/// Веха 18.1–18.3: POSIX-персоналия как сервер. Программа-клиент пользуется только POSIX-подобными
/// вызовами (`open/write/close/read/stat/unlink/readdir`, режимы `O_APPEND/O_TRUNC`) через тонкий
/// IPC-shim и «не знает», что под ней VOID. Файлы персистятся через store (корни-имена + индекс
/// каталога `.dir`), `unlink` честно снимает корень (объект уходит в GC).
fn posix_demo() {
    use void_abi::Rights;

    println!("  [proc] POSIX-персоналия (open/write/read/close/stat/unlink/readdir) через IPC:");
    // Персоналии — cap на store (файлы персистятся под корнями-именами, Веха 18.2).
    let server = proc::spawn("posixfs", user::posix_server_entry(), 0);
    let scap = cap::mint(proc::domain(server), cap::Target::Store, Rights::READ.union(Rights::WRITE));
    proc::set_arg(server, scap.bits() as usize);
    let client = proc::spawn("posix-app", user::posix_client_entry(), 0);
    let ep = cap::mint(proc::domain(client), cap::Target::Endpoint(server), Rights::SEND);
    proc::set_arg(client, ep.bits() as usize);
    println!(
        "    P{} '{}' [cap store] ← клиент P{} '{}' [cap эндпоинт]",
        server, cap::domain_name(proc::domain(server)), client, cap::domain_name(proc::domain(client)),
    );
    proc::run();
    println!("  [proc] сессия персоналии завершена — обратно в ядро");
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
        // Немного занятости, чтобы дать таймеру шанс вытеснить между записями.
        let mut acc = 0u64;
        for k in 0..250_000u64 {
            acc = acc.wrapping_add(k);
        }
        let _ = acc;
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
        unsafe { core::arch::asm!("wfi") }
    }
}
