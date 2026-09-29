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

mod ipc;
mod obj;
mod task;
mod mem;
mod dev;
mod sys;

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
    if ipc::dispatch(t, cur, num) {
        return;
    }
    if obj::dispatch(t, cur, num) {
        return;
    }
    if task::dispatch(t, cur, num) {
        return;
    }
    if mem::dispatch(t, cur, num) {
        return;
    }
    if dev::dispatch(t, cur, num) {
        return;
    }
    if sys::dispatch(t, cur, num) {
        return;
    }
    // Номер не подошёл ни одной группе — честный отказ, а не молчание.
    let f = &mut t.procs[cur].frame;
    vprintln!("  [proc] неизвестный syscall {}", num);
    f.set_ret(usize::MAX);
    f.advance();
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
