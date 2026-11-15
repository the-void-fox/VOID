//! `trace` — **спросить систему, чем она занята** (Веха 231).
//!
//! ```text
//! trace calls 3         какие системные вызовы и сколько времени ядра на них ушло
//! trace sample 3        где горит процессор: адреса, по которым застали код
//! trace events 2        сырой хвост: что именно происходило, по порядку
//! ```
//!
//! ## Зачем, если есть журнал и `fps`
//!
//! Все предыдущие расследования производительности в VOID начинались с того, что счётчики
//! дописывались руками в подозреваемое место — и трижды подряд подозрение оказывалось неверным
//! (последний раз в загадке полноэкранного кадра: «нас вытесняют», «общая память медленная»,
//! «виновато окно приветствия» — все три умерли на числах). Инструмент нужен такой, которому
//! вопрос задают ПОСЛЕ сбора данных, а не до.
//!
//! ## Две половины, и путать их нельзя
//!
//! - **`calls`** — события: кто звал ядро, как часто и сколько ядро на это потратило. Это
//!   ответ на «кто кого ждёт» и «что происходит слишком часто».
//! - **`sample`** — отсчёты: в момент тика процессор был вот по этому адресу. Это ответ на
//!   «где горит время», и только он находит горячий цикл внутри программы, потому что внутри
//!   такого цикла системных вызовов нет вовсе.
//!
//! Инструмент, умеющий одно, молча врёт про другое: горячий цикл не видно по вызовам, а
//! ожидание не видно по выборке.
//!
//! ## Адреса, а не имена
//!
//! Выборка печатает АДРЕСА. Имён у нас нет: таблицу символов в образ мы не кладём (это
//! мегабайты в системе, где образ едет по сети), а гадать по адресу нельзя. Перевод делается на
//! хосте, где лежат те же самые ELF'ы: `Code/tools/ksyms.py` — отдаёт `имя+смещение`.
//! Это та же сделка, что у `dump_lx_trace`: ядро печатает номера, человек расшифровывает их
//! инструментом.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;
use void_user::trace;

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Сколько записей вычерпываем за один заход. Кольцо ядра — 4096; берём четверть, чтобы
/// вычерпывание было частым и дешёвым, а не одним большим рывком.
const BATCH: usize = 1024;

/// Как часто вычерпывать, мс (в вызов уходит нс). Кольцо на 4096 записей при сотнях вызовов в секунду живёт секунды,
/// так что пятая доля секунды — с запасом.
const DRAIN_MS: u64 = 200;

/// Сколько строк показывать в таблицах. Длинный хвост в терминале не читается.
const TOP: usize = 12;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let argv = sys::argv::Argv::take();
    let mode = argv.str(0).unwrap_or("");
    let seconds = argv.str(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);

    let sysview = sys::win::cap_or_grant(13);
    if sysview == sys::NO_CAP {
        sys::write("trace: нет права обзора (sysview)\n".as_bytes());
        sys::write("выдаётся строкой `desktop sysview trace` в конфиге поколения\n".as_bytes());
        sys::exit(1);
    }

    let tags: &[u16] = match mode {
        "calls" => &[trace::TAG_SYSCALL, trace::TAG_LINUX],
        "sample" => &[trace::TAG_SAMPLE_U, trace::TAG_SAMPLE_K],
        "events" => {
            &[trace::TAG_SYSCALL, trace::TAG_LINUX, trace::TAG_SAMPLE_U, trace::TAG_SAMPLE_K]
        }
        _ => {
            usage();
            sys::exit(2);
        }
    };

    if !trace::enable(sysview, trace::mask_of(tags)) {
        sys::write("trace: ядро не включило трассировку (нужен sysview:w)\n".as_bytes());
        sys::exit(1);
    }

    let collected = collect(sysview, seconds);
    // Выключаем ВСЕГДА, в том числе если дальше печать упадёт: оставить систему
    // трассирующей после выхода инструмента — это молчаливый расход, который потом ищут.
    trace::enable(sysview, 0);

    let (_, records, lost_count) = trace::stats(sysview).unwrap_or((0, 0, 0));
    match mode {
        "calls" => print_calls(&collected, sysview, seconds),
        "sample" => print_sample(&collected, sysview, seconds),
        _ => print_events(&collected),
    }
    // Потери печатаем ПОСЛЕ таблицы и всегда, когда они есть: таблица, посчитанная по неполным
    // данным, выглядит так же убедительно, как полная.
    if lost_count > 0 {
        sys::write(
            format!(
                "\nВНИМАНИЕ: {} записей потеряно (кольцо ядра переполнилось) — числа НЕПОЛНЫЕ\n",
                lost_count
            )
            .as_bytes(),
        );
    }
    sys::write(format!("записей в ядре за замер: {}\n", records).as_bytes());
    sys::exit(0);
}

fn usage() {
    sys::write("trace: спросить систему, чем она занята\n\n".as_bytes());
    sys::write("  trace calls [секунд]     какие вызовы и сколько времени ядра они заняли\n".as_bytes());
    sys::write("  trace sample [секунд]    где горит процессор (адреса кода)\n".as_bytes());
    sys::write("  trace events [секунд]    сырой хвост по порядку\n\n".as_bytes());
    sys::write("Адреса из `выборки` переводит в имена хостовый `Code/tools/ksyms.py`.\n".as_bytes());
}

/// Вычерпывать кольцо, пока не выйдет время замера.
fn collect(sysview: usize, seconds: u64) -> Vec<trace::Event> {
    let until = sys::monotonic_ns() + seconds * 1_000_000_000;
    let mut all: Vec<trace::Event> = Vec::new();
    let mut raw = vec![0u8; BATCH * trace::REC];
    let mut ev = vec![trace::Event::default(); BATCH];
    while sys::monotonic_ns() < until {
        sys::sleep_ns(DRAIN_MS * 1_000_000);
        loop {
            let n = trace::read(sysview, &mut raw, &mut ev);
            all.extend_from_slice(&ev[..n]);
            // Меньше полного пакета — кольцо вычерпано досуха, больше читать нечего.
            if n < BATCH {
                break;
            }
        }
    }
    all
}

/// Имена процессов по номеру — чтобы в таблицах стояло `wm`, а не `P7`.
fn names(sysview: usize) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; sys::PROC_REC * 96];
    let Some(total) = sys::proc_list(sysview, &mut buf) else { return out };
    let shown_count = total.min(buf.len() / sys::PROC_REC);
    for k in 0..shown_count {
        let r = &buf[k * sys::PROC_REC..(k + 1) * sys::PROC_REC];
        let pid = u16::from_le_bytes([r[0], r[1]]);
        let len = r[7] as usize;
        let name = core::str::from_utf8(&r[40..40 + len.min(24)]).unwrap_or("?");
        out.push((pid, String::from(name)));
    }
    out
}

fn proc_name(list: &[(u16, String)], pid: u16) -> String {
    match list.iter().find(|(p, _)| *p == pid) {
        Some((_, n)) => format!("{n}"),
        None => format!("P{pid}"),
    }
}

/// Одна строка сводки вызовов: тег, номер, сколько, нс всего, нс худший, кто звал.
struct Row {
    tag: u16,
    nr: u64,
    count: u64,
    sum: u64,
    worst: u64,
    /// Звонившие и сколько раз каждый. Их единицы — список дешевле словаря.
    callers: Vec<(u16, u64)>,
}

/// `trace calls` — сводка по номерам вызовов: сколько раз, сколько времени ЯДРА и КТО звал.
///
/// Веха 233.20 — столбец «кто». Без него таблица называет вызов, но не виновника: «TIME
/// триста раз в секунду» — это вопрос, а не ответ, пока не видно, чей это цикл. Имя берём у
/// того, кто звал чаще всех, и рядом ставим его долю: когда зовут двое, одно имя врало бы.
fn print_calls(ev: &[trace::Event], sysview: usize, seconds: u64) {
    let names = names(sysview);
    let mut summary: Vec<Row> = Vec::new();
    for e in ev {
        if e.tag != trace::TAG_SYSCALL && e.tag != trace::TAG_LINUX {
            continue;
        }
        match summary.iter_mut().find(|r| r.tag == e.tag && r.nr == e.a) {
            Some(row) => {
                row.count += 1;
                row.sum += e.b;
                row.worst = row.worst.max(e.b);
                match row.callers.iter_mut().find(|(p, _)| *p == e.pid) {
                    Some(c) => c.1 += 1,
                    None => row.callers.push((e.pid, 1)),
                }
            }
            None => summary.push(Row {
                tag: e.tag,
                nr: e.a,
                count: 1,
                sum: e.b,
                worst: e.b,
                callers: vec![(e.pid, 1)],
            }),
        }
    }
    if summary.is_empty() {
        sys::write("вызовов не было вовсе — система стояла\n".as_bytes());
        return;
    }
    // Сортируем по ЧИСЛУ вызовов, а не по времени ядра: вопрос «что происходит слишком часто»
    // задают чаще, чем «что дорого», и холостой цикл виден именно числом.
    summary.sort_by(|a, b| b.count.cmp(&a.count));
    let total_calls: u64 = summary.iter().map(|r| r.count).sum();
    let total_ns: u64 = summary.iter().map(|r| r.sum).sum();
    sys::write(
        format!(
            "ВЫЗОВЫ за {} с: {} штук, {} мкс работы ядра ({} вызовов/с)\n\n",
            seconds,
            total_calls,
            total_ns / 1000,
            total_calls / seconds,
        )
        .as_bytes(),
    );
    sys::write("  вызов              сколько   в секунду   ядро, мкс   кто звал\n".as_bytes());
    for row in summary.iter().take(TOP) {
        let name = if row.tag == trace::TAG_LINUX {
            format!("linux #{}", row.nr)
        } else {
            format!("{}", syscall_name(row.nr))
        };
        // Кто звал: самый частый и его доля. Доля важнее имени — она говорит, один это цикл
        // или несколько, а значит, лечить одно место или искать дальше.
        let top = row.callers.iter().max_by_key(|(_, n)| *n);
        let who = match top {
            Some((pid, n)) if *n * 10 >= row.count * 9 => proc_name(&names, *pid),
            Some((pid, n)) => {
                format!("{} ({}%, всего {})", proc_name(&names, *pid), n * 100 / row.count, row.callers.len())
            }
            None => String::from("?"),
        };
        sys::write(
            format!(
                "  {:<18} {:>7}   {:>9}   {:>9}   {}\n",
                name,
                row.count,
                row.count / seconds.max(1),
                row.sum / 1000,
                who,
            )
            .as_bytes(),
        );
    }
    sys::write(
        "\nВремя — РАБОТА ЯДРА по вызову, а не ожидание зовущего: у блокирующих вызовов\n\
         (CALL, RECV, чтение ввода) ядро отвечает быстро и усыпляет процесс.\n"
            .as_bytes(),
    );
}

/// `trace sample` — где застали код. Разделяем процессы и ядро: лечатся они по-разному.
fn print_sample(ev: &[trace::Event], sysview: usize, seconds: u64) {
    let list = names(sysview);
    let mut by_proc: Vec<(u16, u64)> = Vec::new();
    let mut addrs: Vec<(u16, u64, u64)> = Vec::new(); // (pid|0=ядро, адрес, сколько)
    let mut kernel = 0u64;
    let mut total = 0u64;
    for e in ev {
        let in_kernel = e.tag == trace::TAG_SAMPLE_K;
        if !in_kernel && e.tag != trace::TAG_SAMPLE_U {
            continue;
        }
        total += 1;
        if in_kernel {
            kernel += 1;
        } else {
            match by_proc.iter_mut().find(|(p, _)| *p == e.pid) {
                Some(row) => row.1 += 1,
                None => by_proc.push((e.pid, 1)),
            }
        }
        // Адреса сводим по 16 байт: разные команды одного цикла — это одно место, а не десять.
        let key = e.a & !0xf;
        let owner = if in_kernel { u16::MAX } else { e.pid };
        match addrs.iter_mut().find(|(p, a, _)| *p == owner && *a == key) {
            Some(row) => row.2 += 1,
            None => addrs.push((owner, key, 1)),
        }
    }
    if total == 0 {
        sys::write("отсчётов нет: за это время никого не вытесняли — система спала\n".as_bytes());
        return;
    }
    by_proc.sort_by(|a, b| b.1.cmp(&a.1));
    addrs.sort_by(|a, b| b.2.cmp(&a.2));
    sys::write(
        format!(
            "ВЫБОРКА за {} с: {} отсчётов ({} в секунду), из них в ядре {} %\n\n",
            seconds,
            total,
            total / seconds,
            kernel * 100 / total,
        )
        .as_bytes(),
    );
    sys::write("  КТО ЖЖЁТ ПРОЦЕССОР\n".as_bytes());
    for (pid, n) in by_proc.iter().take(TOP) {
        sys::write(
            format!(
                "  {:<20} {:>6} отсчётов  {:>3} %\n",
                proc_name(&list, *pid),
                n,
                n * 100 / total
            )
            .as_bytes(),
        );
    }
    sys::write("\n  ГДЕ ИМЕННО (адрес кода, перевести — tools/ksyms.py)\n".as_bytes());
    for (owner, addr, n) in addrs.iter().take(TOP) {
        let who = if *owner == u16::MAX {
            String::from("ядро")
        } else {
            proc_name(&list, *owner)
        };
        sys::write(
            format!("  {:<14} {:#018x}  {:>5}  {:>3} %\n", who, addr, n, n * 100 / total)
                .as_bytes(),
        );
    }
    sys::write(
        format!(
            "\nОтсчёт берётся на тике вытеснения (квант 20 мс), то есть ~50 в секунду на ядро.\n\
             Цикл короче этого в выборку не попадёт — за ним идти с `calls` или со счётчиком.\n"
        )
        .as_bytes(),
    );
}

/// `trace events` — сырой хвост. Нужен, когда вопрос не «сколько», а «в каком порядке».
fn print_events(ev: &[trace::Event]) {
    let tail = ev.len().saturating_sub(64);
    sys::write(format!("СОБЫТИЯ: всего {}, ниже последние {}\n\n", ev.len(), ev.len() - tail).as_bytes());
    for e in &ev[tail..] {
        let ms = e.t_ns / 1_000_000;
        let us = (e.t_ns % 1_000_000) / 1000;
        match e.tag {
            trace::TAG_SYSCALL => sys::write(
                format!(
                    "  {:>6}.{:03} P{:<3} ц{} {} → {} мкс\n",
                    ms,
                    us,
                    e.pid,
                    e.cpu,
                    syscall_name(e.a),
                    e.b / 1000
                )
                .as_bytes(),
            ),
            trace::TAG_LINUX => sys::write(
                format!("  {:>6}.{:03} P{:<3} ц{} linux #{} → {}\n", ms, us, e.pid, e.cpu, e.a, e.b as i64)
                    .as_bytes(),
            ),
            trace::TAG_SAMPLE_U => sys::write(
                format!("  {:>6}.{:03} P{:<3} ц{} код {:#x}\n", ms, us, e.pid, e.cpu, e.a).as_bytes(),
            ),
            trace::TAG_SAMPLE_K => sys::write(
                format!("  {:>6}.{:03} ядро  ц{} код {:#x}\n", ms, us, e.cpu, e.a).as_bytes(),
            ),
            _ => {}
        }
    }
}

/// Имена вызовов VOID. Таблица снята С КОНСТАНТ библиотеки (`SYS_*` в `lib.rs`), а не набрана
/// по памяти: подписать вызов чужим именем — худший вид вранья в измерителе. Незнакомый номер
/// печатается номером, и это тоже ответ: значит вызов новый, а инструмент не обновили.
fn syscall_name(n: u64) -> String {
    let s = match n {
        1 => "WRITE",
        2 => "EXIT",
        3 => "YIELD",
        4 => "RECV",
        5 => "CALL",
        6 => "REPLY",
        7 => "BLK_READ",
        8 => "OBJ_PUT",
        9 => "OBJ_GET",
        10 => "OBJ_SET_ROOT",
        11 => "OBJ_GET_ROOT",
        12 => "BLK_WRITE",
        13 => "OBJ_DEL_ROOT",
        14 => "READ",
        15 => "EXEC",
        16 => "CAP_DERIVE",
        17 => "MAP",
        18 => "ARGS",
        19 => "STARTCAP",
        20 => "NET_SEND",
        21 => "NET_RECV",
        22 => "NET_MAC",
        23 => "THREAD_SPAWN",
        24 => "THREAD_EXIT",
        25 => "THREAD_JOIN",
        26 => "FUTEX",
        27 => "SET_TLS",
        28 => "CHECKPOINT",
        29 => "RESTORE",
        30 => "INSTALL",
        31 => "MMIO_MAP",
        32 => "DMA_ALLOC",
        33 => "IRQ_WAIT",
        34 => "OBJ_LIST_ROOTS",
        35 => "LOG",
        36 => "TIME",
        37 => "RANDOM",
        38 => "OBJ_PUT_NODE",
        39 => "OBJ_CHILDREN",
        40 => "VIDEO_INFO",
        41 => "SPAWN",
        42 => "WAIT",
        43 => "SELF_ENDPOINT",
        44 => "POWEROFF",
        45 => "KILL",
        46 => "OBJ_GC",
        47 => "SLEEP",
        48 => "PARENT",
        49 => "MOUSE_READ",
        50 => "KLOG",
        51 => "KEY_READ",
        52 => "CONSIZE",
        53 => "SETENV",
        54 => "SHM_NEW",
        55 => "SHM_MAP",
        56 => "SHM_UNMAP",
        57 => "KEYMAP",
        58 => "CAP_INFO",
        59 => "PROC_LIST",
        60 => "PROC_CAPS",
        61 => "PROC_STAT",
        62 => "PROC_REVOKE",
        63 => "SYSINFO",
        64 => "NETDEV",
        65 => "PCI_CFG",
        66 => "HWPROBE",
        67 => "INTERRUPT",
        68 => "INTERRUPTED",
        69 => "PROC_GRANT",
        70 => "SELF",
        71 => "VIDEO_FLUSH",
        72 => "PEER_ROOT",
        73 => "CAP_NARROW",
        74 => "CLOCK_SET",
        75 => "TRACE",
        _ => return format!("#{n}"),
    };
    String::from(s)
}
