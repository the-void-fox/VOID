//! Веха 214.2 — **трансля́тор Linux-syscall'ов**: та его часть, которой нужна таблица процессов.
//!
//! Чистые части linux-персоналии (коды ошибок, разбор номеров, раскладка стартового стека,
//! заполнение `stat`/`utsname`) давно живут в [`crate::linux`] — они ни от чего не зависят.
//! Здесь всё остальное: сам диспетчер, дескрипторы, трубы и отображения — то, что работает
//! ТАБЛИЦЕЙ ПРОЦЕССОВ и потому раньше лежало в `proc.rs`.
//!
//! **Почему вынесено (Веха 214.2).** `proc.rs` дорос до 6867 строк — четверти ядра, — и 57 %
//! этого приходилось на две функции: диспетчер VOID и диспетчер Linux. Файл резали не за
//! размер, а за то, что в такую функцию нельзя заглянуть целиком. Linux-ABI ушёл первым, потому
//! что он самый обособленный: чужое соглашение, свои структуры, почти не переплетён с остальным.
//!
//! **Почему ПОДМОДУЛЬ `proc`, а не сосед.** Всё здесь работает приватными полями `Table` и
//! `Proc`. Вынеси это соседним модулем — пришлось бы открыть их наружу, то есть разменять
//! границу инкапсуляции на расположение файлов. Потомок же видит приватное предка по правилам
//! языка, и граница остаётся там, где была: наружу из `proc` по-прежнему торчит только то,
//! что торчало.

use super::*;

// ─── linux-abi: трансля́тор syscall'ов (Веха 38) ───────────────────────────────

/// Прочитать 2 байта по `user_pc` и проверить, что это `syscall` (0F 05) — распознаёт
/// #UD от linux-процесса на x86-64 (см. [`handle_user_trap`]). Читаем побайтно с трансляцией:
/// инструкция теоретически может лежать на стыке страниц.
pub(super) fn is_linux_syscall_insn(t: &Table, cur: usize) -> bool {
    let pc = t.procs[cur].frame.user_pc();
    let root = arch::space_root(t.procs[cur].space);
    let byte = |va: usize| -> Option<u8> {
        // Веха 87: translate отдаёт ФИЗИЧЕСКИЙ адрес — читаем его через direct-map.
        arch::translate(root, va).map(|pa| unsafe { *(crate::frame::ptr(pa) as *const u8) })
    };
    byte(pc) == Some(0x0f) && byte(pc + 1) == Some(0x05)
}

/// Записать `data` в буфер процесса `cur` по VA `va` (доотобразив ленивую кучу). `false` —
/// адрес недоступен (фреймы кончились). Процесс — current, поэтому пишем прямой ссылкой
/// (riscv: SUM=1; x86: ring0 пишет U-страницы, SMAP не включён).
fn lx_put(t: &mut Table, cur: usize, va: usize, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    if !ensure_heap_range(t, cur, va, data.len()) {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), va as *mut u8, data.len()) };
    true
}

/// Прочитать срез памяти процесса `cur` длиной `len` по VA `va` для ядра. Возвращает `None`,
/// если диапазон не удалось обеспечить. Процесс — current (см. [`lx_put`]).
fn lx_get<'a>(t: &mut Table, cur: usize, va: usize, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if !ensure_heap_range(t, cur, va, len) {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(va as *const u8, len) })
}

/// Веха 108.3 — прочитать NUL-терминированную строку (путь) из памяти процесса. Потолок стоит
/// затем, что длину задаёт чужая программа, а искать нуль до конца адресного пространства нельзя.
pub(super) fn lx_cstr(t: &mut Table, cur: usize, va: usize, max: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for i in 0..max {
        let b = lx_get(t, cur, va + i, 1)?[0];
        if b == 0 {
            return Some(out);
        }
        out.push(b);
    }
    None
}

// ─── трубы (Веха 183) ────────────────────────────────────────────────────────────────────────

/// Разбудить всех, кто ждёт на трубе `idx`.
///
/// Будим ВСЕХ, а не одного: спящих у трубы может быть и читатель, и писатель, и кто из них
/// дождался — решает не будильник, а повторённый системный вызов. Проснувшийся, которому опять
/// нечего делать, просто уснёт снова — это дешевле, чем ошибиться в выборе.
fn pipe_wake(t: &mut Table, idx: usize) {
    for i in 0..t.procs.len() {
        if t.procs[i].state == State::PipeWait(idx) {
            t.procs[i].state = State::Runnable;
            t.procs[i].ready_at = arch::now_ticks();
        }
    }
}

/// Занять слот трубы. Возвращает её номер.
fn pipe_alloc(t: &mut Table) -> usize {
    let p = LxPipe { buf: alloc::collections::VecDeque::new(), readers: 1, writers: 1 };
    match t.lx_pipes.iter().position(|s| s.is_none()) {
        Some(i) => {
            t.lx_pipes[i] = Some(p);
            i
        }
        None => {
            t.lx_pipes.push(Some(p));
            t.lx_pipes.len() - 1
        }
    }
}

/// Закрыть один конец трубы. Последний закрытый конец освобождает слот.
fn pipe_close_end(t: &mut Table, idx: usize, write_end: bool) {
    let Some(Some(p)) = t.lx_pipes.get_mut(idx) else { return };
    if write_end {
        p.writers = p.writers.saturating_sub(1);
    } else {
        p.readers = p.readers.saturating_sub(1);
    }
    let dead = p.readers == 0 && p.writers == 0;
    // Разбудить обязательно: читатель, у которого закрылся последний писатель, ждёт НЕ данных,
    // а конца файла, и узнать о нём может только проснувшись.
    pipe_wake(t, idx);
    if dead {
        t.lx_pipes[idx] = None;
    }
}

/// Веха 186 — отпустить ОДИН дескриптор личности Linux. Конец трубы уходит счётчику, файл на
/// запись — в store (уносит его последний держатель).
///
/// Функция общая для трёх мест, где дескриптор исчезает: `close`, `dup2` поверх занятого номера
/// и уборка за умершим. Порознь их писать нельзя: забыть здесь трубу — значит навсегда оставить
/// читателя без конца файла, забыть файл — значит потерять всё, что в него написали.
/// `false` — файл не удалось записать (нет каталога или места).
fn lx_release_fd(t: &mut Table, pid: usize, sl: LxFd) -> bool {
    if let Some((idx, write_end)) = sl.pipe {
        pipe_close_end(t, idx, write_end);
    }
    let Some(wi) = sl.wfile else { return true };
    let last = match t.lx_wfiles.get_mut(wi).and_then(|w| w.as_mut()) {
        Some(w) => {
            w.holders = w.holders.saturating_sub(1);
            w.holders == 0
        }
        None => return true,
    };
    if !last {
        return true;
    }
    let w = t.lx_wfiles[wi].take().expect("держатель был");
    if crate::lxfs::write_file(&w.path, &w.buf) {
        return true;
    }
    vprintln!(
        "  [linux] P{} файл '{}' НЕ записан ({} Б) — нет каталога или места",
        pid,
        core::str::from_utf8(&w.path).unwrap_or("?"),
        w.buf.len(),
    );
    false
}

/// Веха 186 — отпустить ВСЕ дескрипторы умирающего процесса.
///
/// Без этого конвейер зависает намертво, и виновата не труба: `sh` закрывает свои концы, а
/// потомки уносят свои в могилу молча. Счётчик писателей не доходит до нуля, читатель ждёт
/// конца файла, которого никто не объявит. `execve` сюда НЕ ходит: там процесс не умирает, а
/// меняет образ, и дескрипторы обязаны его пережить.
pub(super) fn lx_close_all(t: &mut Table, pid: usize) {
    if t.procs[pid].lx_fds.is_empty() {
        return;
    }
    let fds: Vec<LxFd> = t.procs[pid].lx_fds.iter_mut().filter_map(|s| s.take()).collect();
    for sl in fds {
        let _ = lx_release_fd(t, pid, sl);
    }
}

/// Веха 182 — `execve`: заменить образ ТЕКУЩЕГО процесса, не заводя нового.
///
/// ## Почему это не `SYS_SPAWN` с последующим `exit`
///
/// `execve` обязан сохранить НОМЕР процесса и его дескрипторы: на этом стоит вся оболочка —
/// родитель ждёт того же ребёнка, которого породил, а перенаправление (`> файл`) делается ДО
/// `execve` и обязано пережить его. Спавн с самоубийством дал бы другой pid и потерянные
/// дескрипторы, и ошибка вылезла бы не здесь, а в `wait4` через полсборки.
///
/// ## Порядок, в котором нельзя ошибиться
///
/// Новый образ строится в НОВОМ адресном пространстве и только потом подменяет старое. Если по
/// дороге что-то не вышло — не нашёлся файл, негодный ELF, кончилась память, — старое остаётся
/// нетронутым, и вызывающий получает честный `errno`. Так и ведёт себя Linux: неудачный
/// `execve` возвращается, удачный — нет.
///
/// Старое пространство освобождается НЕ ЗДЕСЬ, а через `dead_roots`: на нём прямо сейчас стоит
/// это самое ядро машины, и снести его под собой значило бы освободить таблицы, на которые
/// смотрит регистр. Планировщик отдаст фреймы, когда никто на них не стоит (Веха 170).
fn exec_linux_in_place(
    t: &mut Table,
    cur: usize,
    pname: &'static str,
    bytes: &[u8],
    args_blob: Vec<u8>,
    env_blob: Vec<u8>,
) -> bool {
    let Some(root) = new_address_space() else { return false };
    let fail = |root: usize| {
        unsafe { arch::free_address_space(root) };
        false
    };
    let pie = match elf::load_pie(root, bytes, USER_REGION_START, USER_HEAP_BASE_VA) {
        Ok(p) => p,
        Err(e) => {
            vprintln!("  [linux] execve: негодный PIE-образ: {:?}", e);
            return fail(root);
        }
    };
    // Динамический бинарь: загрузчик из `PT_INTERP` — тем же путём, что и при спавне.
    let mut interp_base = 0usize;
    let mut entry = pie.entry;
    if let Some(ipath) = elf::interp_path(bytes) {
        let Some(meta) = crate::lxfs::lookup(ipath) else { return fail(root) };
        let Some(idata) = crate::lxfs::read_all(&meta) else { return fail(root) };
        match elf::load_pie(root, &idata, INTERP_BASE_VA, USER_HEAP_BASE_VA) {
            Ok(ip) => {
                interp_base = INTERP_BASE_VA;
                entry = ip.entry;
            }
            Err(_) => return fail(root),
        }
    }
    let mut rnd = [0u8; 16];
    let mut seed = arch::now_ticks();
    for chunk in rnd.chunks_mut(8) {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let b = seed.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    let (block, sp) = crate::linux::build_init_stack(
        USER_STACK_TOP_VA,
        &args_blob,
        &env_blob,
        &pie,
        rnd,
        interp_base,
    );
    copy_to_space(root, sp, &block);

    // С этой строки пути назад нет: образ подменён.
    let old = arch::space_root(t.procs[cur].space);
    t.procs[cur].space = arch::space_token(root);
    t.procs[cur].frame = TrapFrame::new_user(entry, sp, 0);
    t.procs[cur].heap_brk = USER_HEAP_BASE_VA; // куча новая — старого `brk` больше нет
    t.procs[cur].args = args_blob;
    t.procs[cur].env = env_blob;
    t.procs[cur].linux = true;
    t.procs[cur].image = None;
    // Дескрипторы ПЕРЕЖИВАЮТ `execve` — так велит POSIX, и на этом стоит перенаправление вывода:
    // заведи таблицу заново, и всё, что оболочка настроила ДО подмены образа, пропало бы, а ради
    // этого она и ветвится. Уходят только помеченные `FD_CLOEXEC` — те, что новому образу не
    // предназначались (сохранённый stdout оболочки, чужие концы трубы).
    //
    // А вот ДОМЕН ПРАВ не меняется, и это не забывчивость. В VOID домен привязан к ИМЕНИ
    // программы (Веха 156), и подменять его здесь значило бы выдать процессу права чужой
    // программы по одному лишь её названию — то есть отдать повышение прав любому, кто умеет
    // звать `execve`. Права наследуются от того, кто был, ровно как при спавне ребёнка.
    let doomed: Vec<LxFd> = t.procs[cur]
        .lx_fds
        .iter_mut()
        .filter(|s| s.as_ref().map(|f| f.cloexec).unwrap_or(false))
        .filter_map(|s| s.take())
        .collect();
    for sl in doomed {
        let _ = lx_release_fd(t, cur, sl);
    }
    let _ = pname; // имя процесса живёт в `args[0]`, отдельного поля у него нет
    t.dead_roots.push(old);
    true
}

/// Веха 186 — что за дескриптор: консоль, труба или файл. Один вопрос вместо трёх разных
/// условий, разбросанных по обработчикам.
#[derive(Clone, Copy, PartialEq)]
enum FdKind {
    None,
    Console,
    Pipe(usize, bool),
    File,
}

fn fd_kind(t: &Table, cur: usize, fd: usize) -> FdKind {
    match t.procs[cur].lx_fds.get(fd).and_then(|s| s.as_ref()) {
        None => FdKind::None,
        Some(f) if f.console => FdKind::Console,
        Some(f) => match f.pipe {
            Some((i, w)) => FdKind::Pipe(i, w),
            None => FdKind::File,
        },
    }
}

/// `O_CLOEXEC` — один и тот же бит у `open`, `pipe2` и `dup3`.
const O_CLOEXEC: usize = 0o2000000;

/// Веха 186 — скопировать дескриптор `old`: в НАЗВАННЫЙ номер (`to = Some`) или в младший
/// свободный, начиная с `min`.
///
/// Общая половина `dup`, `dup2`/`dup3` и `fcntl(F_DUPFD)` — трёх обличий одного действия. Порознь
/// их писать нельзя: у трубы и у файла на запись копия прибавляет ДЕРЖАТЕЛЯ, и место, где про это
/// забыли, обнаруживается не отказом, а зависшим конвейером или потерянным файлом.
fn lx_dup_fd(
    t: &mut Table,
    cur: usize,
    old: usize,
    to: Option<usize>,
    min: usize,
    cloexec: bool,
) -> usize {
    let Some(mut copy) = t.procs[cur].lx_fds.get(old).cloned().flatten() else {
        return crate::linux::err(crate::linux::EBADF);
    };
    // dup2(x, x) — тождество, а не работа: закрывать при этом нельзя (POSIX особо оговаривает).
    if to == Some(old) {
        return old;
    }
    copy.cloexec = cloexec;
    if let Some((idx, write_end)) = copy.pipe {
        if let Some(Some(p)) = t.lx_pipes.get_mut(idx) {
            if write_end {
                p.writers += 1;
            } else {
                p.readers += 1;
            }
        }
    }
    if let Some(wi) = copy.wfile {
        if let Some(Some(w)) = t.lx_wfiles.get_mut(wi) {
            w.holders += 1;
        }
    }
    match to {
        Some(newfd) => {
            // Занятый номер сперва ЗАКРЫВАЕТСЯ — так велит POSIX, и на этом стоит
            // перенаправление: `dup2(труба, 1)` обязан убрать прежний stdout, иначе тот остался
            // бы держателем.
            if let Some(Some(prev)) = t.procs[cur].lx_fds.get_mut(newfd).map(|s| s.take()) {
                let _ = lx_release_fd(t, cur, prev);
            }
            let tbl = &mut t.procs[cur].lx_fds;
            while tbl.len() <= newfd {
                tbl.push(None);
            }
            tbl[newfd] = Some(copy);
            newfd
        }
        None => {
            let tbl = &mut t.procs[cur].lx_fds;
            while tbl.len() <= min {
                tbl.push(None);
            }
            match tbl.iter().skip(min).position(|sl| sl.is_none()) {
                Some(i) => {
                    tbl[min + i] = Some(copy);
                    min + i
                }
                None => {
                    tbl.push(Some(copy));
                    tbl.len() - 1
                }
            }
        }
    }
}

/// Веха 186 — завести процессу стандартные три дескриптора КОНСОЛЬЮ.
///
/// Раньше их не существовало вовсе: 0/1/2 разбирались условиями по номеру. Пока так, `dup2` не
/// имел чего присваивать, а без него у чужого `sh` нет ни перенаправления, ни конвейера.
fn lx_init_stdio(t: &mut Table, pid: usize) {
    let mk = || {
        Some(LxFd {
            meta: crate::lxfs::Meta {
                id: void_abi::ContentId([0u8; 32]),
                size: 0,
                ty: void_tree::K_FILE,
                src: crate::lxfs::Src::Hier,
            },
            off: 0,
            path: Vec::new(),
            dpos: 0,
            wfile: None,
            pipe: None,
            console: true,
            cloexec: false,
        })
    };
    let tbl = &mut t.procs[pid].lx_fds;
    tbl.clear();
    for _ in 0..3 {
        tbl.push(mk());
    }
}

/// Открыть найденный узел: занять слот в таблице дескрипторов процесса, вернуть номер fd.
fn lx_fd_alloc(t: &mut Table, cur: usize, meta: crate::lxfs::Meta, path: Vec<u8>) -> usize {
    let fd =
        LxFd { meta, off: 0, path, dpos: 0, wfile: None, pipe: None, console: false, cloexec: false };
    let tbl = &mut t.procs[cur].lx_fds;
    let i = match tbl.iter().position(|s| s.is_none()) {
        Some(i) => {
            tbl[i] = Some(fd);
            i
        }
        None => {
            tbl.push(Some(fd));
            tbl.len() - 1
        }
    };
    i + LX_FD_BASE
}

/// Первый номер файлового дескриптора Linux-процесса: 0/1/2 заняты консолью.
/// Веха 186 — дескриптор ЕСТЬ индекс в таблице: 0/1/2 занимает консоль, как и положено.
///
/// Раньше здесь стояла тройка, а 0/1/2 обрабатывались условиями по номеру. Ноль оставлен
/// константой, а не убран, ровно затем, чтобы арифметика в двух десятках мест осталась той же и
/// правка не превратилась в переписывание всей личности.
const LX_FD_BASE: usize = 0;

/// Права страницы из `prot` линуксового `mmap`/`mprotect` (PROT_READ=1, WRITE=2, EXEC=4).
///
/// `PROT_NONE` мы отображаем как «читаемо»: ld.so резервирует им дыры между сегментами и потом
/// перекрывает их FIXED-отображениями. Настоящая защита от чтения потребовала бы отдельного
/// состояния «страница есть, но недоступна», а пользы для запуска бинаря не даёт.
fn lx_page_flags(prot: usize) -> usize {
    let mut f = arch::MAP_U | arch::MAP_R;
    if prot & 2 != 0 {
        f |= arch::MAP_W;
    }
    if prot & 4 != 0 {
        f |= arch::MAP_X;
    }
    f
}

/// Материализовать диапазон страниц процесса с правами на ЗАПИСЬ (в них ещё предстоит копировать).
/// `false` — не хватило памяти или квоты.
fn lx_map_range(t: &mut Table, cur: usize, start: usize, size: usize, _prot: usize) -> bool {
    let leader = t.procs[cur].group;
    let root = arch::space_root(t.procs[cur].space);
    let mut va = start;
    while va < start + size {
        // Страница могла остаться от ПРЕДЫДУЩЕГО отображения этого же диапазона (ld.so сперва
        // резервирует весь файл, потом кладёт в него сегменты) — и остаться без права записи.
        // Копировать в такую нечем, поэтому права возвращаем на запись независимо от того,
        // была она отображена или нет.
        if let Some(pa) = arch::translate(root, va) {
            let _ = unsafe {
                arch::map(root, va, pa & !(PAGE - 1), arch::MAP_R | arch::MAP_W | arch::MAP_U)
            };
        }
        if arch::translate(root, va).is_none() {
            if t.procs[leader].pages >= page_quota() {
                return false;
            }
            let Some(pa) = frame::alloc() else { return false };
            if !unsafe { arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) } {
                frame::free(pa);
                return false;
            }
            t.procs[leader].pages += 1;
        }
        va += PAGE;
    }
    // Веха 170 — первая ветка цикла ПЕРЕЗАПИСЫВАЕТ права уже отображённой страницы, значит
    // своего сброса мало (см. `flush_space`).
    flush_space(t, t.procs[cur].space);
    true
}

/// Поставить диапазону страниц права `prot` (уже отображённым — не трогая их содержимого).
fn lx_protect_range(t: &mut Table, cur: usize, start: usize, size: usize, prot: usize) {
    let root = arch::space_root(t.procs[cur].space);
    let flags = lx_page_flags(prot);
    let mut va = start;
    while va < start + size {
        if let Some(pa) = arch::translate(root, va) {
            let _ = unsafe { arch::map(root, va, pa & !(PAGE - 1), flags) };
        }
        va += PAGE;
    }
    // Веха 170 — права УРЕЗАНЫ (mprotect снимает запись с уже отданных страниц): сосед со
    // старой трансляцией продолжал бы туда писать.
    flush_space(t, t.procs[cur].space);
}

/// Веха 38 — трансля́тор Linux-syscall'ов для процессов личности `linux` ([`crate::linux`]).
/// Зеркало VOID-диспетчера [`syscall`], но номера/семантика — Linux; завершённый вызов
/// перешагивает свою инструкцию `skip_syscall_insn` (на riscv это sepc+4, на x86 rip+2),
/// блокирующий (чтение stdin) — оставляет PC на месте для рестарта, как VOID `SYS_READ`.
pub(super) fn linux_syscall(t: &mut Table, cur: usize) {
    use crate::linux::{self, Lx};
    let nr = t.procs[cur].frame.syscall_num();
    let a = |t: &Table, i: usize| t.procs[cur].frame.arg(i);
    let (a0, a1, a2) = (a(t, 0), a(t, 1), a(t, 2));

    let decoded = linux::decode(nr);
    // Значение результата вычисляем в `ret`; блокирующие/завершающие ветки ставят `done=false`
    // и сами разбираются с состоянием/PC (тогда общий эпилог не трогает кадр).
    let mut ret: usize = 0;
    let mut done = true;

    match decoded {
        // ── вывод ──────────────────────────────────────────────────────────────
        Some(Lx::Write) | Some(Lx::Writev) => {
            let fd = a0;
            // Байты сначала СОБИРАЮТСЯ, а дальше судьба у них одна. Разводить `write` и `writev`
            // по разным веткам было ошибкой, и дорогой: буферизованный вывод musl идёт ИМЕННО
            // через `writev` (два куска — свой буфер и хвост), поэтому `ls > файл` не писал ни
            // байта, пока `writev` знал одну лишь консоль. Пишущий этого не видит: вывод в
            // терминал строчный и уходит через `write`, а стоит перенаправить — и тишина.
            let bytes: Option<Vec<u8>> = if decoded == Some(Lx::Write) {
                lx_get(t, cur, a1, a2).map(|b| b.to_vec())
            } else {
                // iov: массив из iovcnt структур { base: u64, len: u64 }.
                let mut out: Vec<u8> = Vec::new();
                let mut ok = true;
                for i in 0..a2 {
                    let Some(ent) = lx_get(t, cur, a1 + i * 16, 16) else {
                        ok = false;
                        break;
                    };
                    let base = usize::from_le_bytes(ent[0..8].try_into().unwrap());
                    let len = usize::from_le_bytes(ent[8..16].try_into().unwrap());
                    match lx_get(t, cur, base, len) {
                        Some(b) => out.extend_from_slice(b),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok.then_some(out)
            };
            let kind = fd_kind(t, cur, fd);
            match bytes {
                None => ret = linux::err(linux::EFAULT),
                Some(bytes) if kind == FdKind::Console => {
                    crate::print!("{}", core::str::from_utf8(&bytes).unwrap_or("<?>"));
                    ret = bytes.len();
                }
                Some(bytes) => {
                    if let FdKind::Pipe(idx, true) = kind {
                        // Веха 183 — запись в ТРУБУ.
                        let (readers, room) = match t.lx_pipes.get(idx).and_then(|p| p.as_ref()) {
                            Some(p) => (p.readers, PIPE_CAP.saturating_sub(p.buf.len())),
                            None => (0, 0),
                        };
                        if readers == 0 {
                            // Читателей не осталось — писать некому. На Linux здесь ещё и
                            // `SIGPIPE`; сигналов у нас нет, остаётся честный код.
                            ret = linux::err(linux::EPIPE);
                        } else if room == 0 && !bytes.is_empty() {
                            // Труба полна: ждём читателя. Кадр и PC НЕ трогаем — в личности
                            // Linux счётчик команд стоит НА самой инструкции `syscall` до
                            // эпилога (`skip_syscall_insn`), так что проснувшийся повторит
                            // вызов сам. `restart()` здесь был бы вторым откатом и увёл бы PC
                            // внутрь предыдущей инструкции.
                            t.procs[cur].state = State::PipeWait(idx);
                            if let Some(nx) = t.next_runnable(cur) {
                                t.set_cur(nx);
                            }
                            done = false;
                            ret = 0;
                        } else {
                            // Короткая запись законна: столько, сколько влезло. Так же ведёт
                            // себя труба на Linux, и вызывающий дозапишет остаток.
                            let n = bytes.len().min(room);
                            if let Some(Some(p)) = t.lx_pipes.get_mut(idx) {
                                p.buf.extend(bytes[..n].iter().copied());
                            }
                            pipe_wake(t, idx);
                            ret = n;
                        }
                    } else {
                        // Веха 181 — запись в ФАЙЛ. Копим в буфере; в store уедет на `close`
                        // одним объектом (или кусками, если вырос).
                        ret = match fd
                            .checked_sub(LX_FD_BASE)
                            .and_then(|i| t.procs[cur].lx_fds.get_mut(i).and_then(|s| s.as_mut()))
                        {
                            Some(sl) => match sl.wfile {
                                Some(wi) => {
                                    let at = sl.off as usize;
                                    let end = at + bytes.len();
                                    sl.off = end as u64;
                                    match t.lx_wfiles.get_mut(wi).and_then(|w| w.as_mut()) {
                                        Some(w) => {
                                            // Дыра от `lseek` за конец — нулями, как велит POSIX.
                                            if w.buf.len() < end {
                                                w.buf.resize(end, 0);
                                            }
                                            w.buf[at..end].copy_from_slice(&bytes);
                                            bytes.len()
                                        }
                                        None => linux::err(linux::EBADF),
                                    }
                                }
                                None => linux::err(linux::EBADF),
                            },
                            None => linux::err(linux::EBADF),
                        };
                    }
                }
            }
        }
        // ── ввод (stdin с консоли, блокирующе) ──────────────────────────────────
        Some(Lx::Read) => {
            let (fd, buf, len) = (a0, a1, a2);
            let kind = fd_kind(t, cur, fd);
            if let FdKind::Pipe(idx, false) = kind {
                // Веха 183 — чтение из ТРУБЫ.
                let (have, writers) = match t.lx_pipes.get(idx).and_then(|p| p.as_ref()) {
                    Some(p) => (p.buf.len(), p.writers),
                    None => (0, 0),
                };
                if have > 0 {
                    let n = have.min(len);
                    let mut tmp = alloc::vec![0u8; n];
                    if let Some(Some(p)) = t.lx_pipes.get_mut(idx) {
                        for b in tmp.iter_mut() {
                            *b = p.buf.pop_front().unwrap_or(0);
                        }
                    }
                    if lx_put(t, cur, buf, &tmp) {
                        // Место освободилось — писателю, который ждал, есть смысл проснуться.
                        pipe_wake(t, idx);
                        ret = n;
                    } else {
                        ret = linux::err(linux::EFAULT);
                    }
                } else if writers == 0 {
                    // Писателей не осталось и данных нет — это КОНЕЦ ФАЙЛА, а не ошибка.
                    ret = 0;
                } else {
                    // Пусто, но писатель жив — ждём. PC не трогаем (см. запись в трубу).
                    t.procs[cur].state = State::PipeWait(idx);
                    if let Some(nx) = t.next_runnable(cur) {
                        t.set_cur(nx);
                    }
                    done = false;
                    ret = 0;
                }
            } else if kind == FdKind::File {
                // Файл из store (Веха 108.3). Чтение короткое: за раз отдаём не больше остатка
                // текущего куска блоба — так же ведёт себя `read` на трубе, и musl дочитает.
                let slot = t.procs[cur].lx_fds.get(fd - LX_FD_BASE).cloned().flatten();
                ret = match slot {
                    None => linux::err(linux::EBADF),
                    Some(f) => {
                        let mut tmp = alloc::vec![0u8; len.min(64 * 1024)];
                        let n = crate::lxfs::read_at(&f.meta, f.off, &mut tmp);
                        if lx_put(t, cur, buf, &tmp[..n]) {
                            if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                                sl.off += n as u64;
                            }
                            n
                        } else {
                            linux::err(linux::EFAULT)
                        }
                    }
                };
            } else if kind != FdKind::Console {
                ret = linux::err(linux::EBADF);
            } else if !ensure_heap_range(t, cur, buf, len) {
                ret = linux::err(linux::EFAULT);
            } else {
                let mut n = 0usize;
                while n < len {
                    let Some(b) = arch::console_getc() else { break };
                    unsafe { *((buf + n) as *mut u8) = b };
                    n += 1;
                }
                if n > 0 || len == 0 {
                    ret = n;
                } else {
                    // Ввода нет — заблокироваться с рестартом (PC на инструкции syscall'а).
                    t.procs[cur].state = State::StdinWait;
                    if let Some(nx) = t.next_runnable(cur) {
                        t.set_cur(nx);
                    }
                    done = false;
                }
            }
        }
        Some(Lx::Pread64) => {
            // pread64(fd, buf, len, off) — им `ld.so` читает заголовки ELF, не двигая позицию.
            let (fd, buf, len, off) = (a0, a1, a2, a(t, 3));
            let slot = fd
                .checked_sub(LX_FD_BASE)
                .and_then(|i| t.procs[cur].lx_fds.get(i).cloned())
                .flatten();
            ret = match slot {
                None => linux::err(linux::EBADF),
                Some(f) => {
                    let mut tmp = alloc::vec![0u8; len.min(64 * 1024)];
                    let n = crate::lxfs::read_at(&f.meta, off as u64, &mut tmp);
                    if lx_put(t, cur, buf, &tmp[..n]) { n } else { linux::err(linux::EFAULT) }
                }
            };
        }
        Some(Lx::Readv) => {
            // Для stdin достаточно наполнить первый непустой iov (короткое чтение допустимо).
            let (fd, iov, iovcnt) = (a0, a1, a2);
            if fd != 0 {
                ret = linux::err(linux::EBADF);
            } else {
                let mut got = 0usize;
                for i in 0..iovcnt {
                    let ent = match lx_get(t, cur, iov + i * 16, 16) {
                        Some(e) => e,
                        None => break,
                    };
                    let base = usize::from_le_bytes(ent[0..8].try_into().unwrap());
                    let len = usize::from_le_bytes(ent[8..16].try_into().unwrap());
                    if len == 0 || !ensure_heap_range(t, cur, base, len) {
                        continue;
                    }
                    while got < len {
                        let Some(b) = arch::console_getc() else { break };
                        unsafe { *((base + got) as *mut u8) = b };
                        got += 1;
                    }
                    if got > 0 {
                        break;
                    }
                }
                ret = got; // 0 = EOF-подобно (не блокируем readv — им пользуются реже)
            }
        }
        // ── память: brk/mmap поверх ленивой кучи процесса ───────────────────────
        Some(Lx::Brk) => {
            let leader = t.procs[cur].group;
            let cur_brk = t.procs[leader].heap_brk;
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            ret = if a0 == 0 {
                cur_brk
            } else if a0 >= USER_HEAP_BASE_VA && a0 <= limit {
                t.procs[leader].heap_brk = a0; // растёт/сжимается лениво (страницы по фолту)
                a0
            } else {
                cur_brk // за пределами — не двигаем (Linux: возврат старого = «не удалось»)
            };
        }
        Some(Lx::Mmap) => {
            // Веха 108.4 — три случая, и все три нужны динамическому загрузчику:
            //   1) анонимное без адреса — хвост ленивой кучи (страницы придут по фолту);
            //   2) MAP_FIXED — материализовать страницы ПО ЗАДАННОМУ адресу с правами `prot`
            //      (ld.so сперва резервирует диапазон, потом кладёт в него сегменты);
            //   3) файловое — то же плюс копирование содержимого из store.
            let (addr, len, prot, flags, fd, off) =
                (a0, a1, a2, a(t, 3), a(t, 4) as isize, a(t, 5));
            const MAP_ANONYMOUS: usize = 0x20;
            const MAP_FIXED: usize = 0x10;
            let anon = flags & MAP_ANONYMOUS != 0 || fd < 0;
            let leader = t.procs[cur].group;
            let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
            let size = (len + PAGE - 1) & !(PAGE - 1);

            // Куда ложимся: заданный адрес (MAP_FIXED) либо хвост кучи.
            let start = if flags & MAP_FIXED != 0 { addr & !(PAGE - 1) } else { t.procs[leader].heap_brk };
            let end = start.saturating_add(size);
            if len == 0 || end > limit || start < USER_HEAP_BASE_VA {
                ret = linux::err(linux::ENOMEM);
            } else {
                // Диапазон обязан числиться кучей: по нему пойдут фолты и проверки шлюзов.
                if end > t.procs[leader].heap_brk {
                    t.procs[leader].heap_brk = end;
                }
                let need_data = !anon;
                // Анонимное без FIXED оставляем ленивым (так было и раньше — дёшево); всё
                // остальное материализуем сразу: под копирование содержимого страницы нужны.
                let ok = if !need_data && flags & MAP_FIXED == 0 {
                    true
                } else {
                    lx_map_range(t, cur, start, size, prot)
                };
                if !ok {
                    ret = linux::err(linux::ENOMEM);
                } else if anon && flags & MAP_FIXED != 0 {
                    // MAP_ANONYMOUS обязано быть НУЛЯМИ, а страницы тут — не обязательно свежие:
                    // ld.so сперва отображает файлом ВЕСЬ образ библиотеки (включая будущий bss),
                    // и только потом накрывает хвост анонимным отображением. Без этого зануления
                    // в bss оставались байты файла — glibc видел там «занятый» замок и вставал
                    // намертво в futex, а printf брал оттуда указатель и падал в #GP.
                    let zero = alloc::vec![0u8; PAGE];
                    let mut done = 0usize;
                    while done < size {
                        let n = (size - done).min(PAGE);
                        if !lx_put(t, cur, start + done, &zero[..n]) {
                            break;
                        }
                        done += n;
                    }
                    lx_protect_range(t, cur, start, size, prot);
                    vprintln!("  [linux] P{} mmap анонимно {:#x}+{:#x} — обнулено", cur, start, size);
                    ret = start;
                } else if need_data {
                    let slot = fd
                        .try_into()
                        .ok()
                        .and_then(|f: usize| f.checked_sub(LX_FD_BASE))
                        .and_then(|i| t.procs[cur].lx_fds.get(i).cloned())
                        .flatten();
                    match slot {
                        None => ret = linux::err(linux::EBADF),
                        Some(f) => {
                            // Копируем файловую часть; хвост до конца страниц остаётся нулевым
                            // (фреймы приходят обнулёнными) — это и есть .bss сегмента.
                            let mut done = 0usize;
                            let mut buf = alloc::vec![0u8; 64 * 1024];
                            while done < len {
                                let take = (len - done).min(buf.len());
                                let n = crate::lxfs::read_at(
                                    &f.meta,
                                    (off + done) as u64,
                                    &mut buf[..take],
                                );
                                if n == 0 {
                                    break;
                                }
                                if !lx_put(t, cur, start + done, &buf[..n]) {
                                    break;
                                }
                                done += n;
                            }
                            // Права ставим ПОСЛЕ копирования: сегмент кода приходит без W, а
                            // писать в него нам было надо.
                            lx_protect_range(t, cur, start, size, prot);
                            vprintln!(
                                "  [linux] P{} mmap файла fd{} off={:#x} len={:#x} → {:#x} prot={} скопировано {:#x}",
                                cur, fd, off, len, start, prot, done
                            );
                            ret = start;
                        }
                    }
                } else {
                    ret = start;
                }
            }
        }
        Some(Lx::Munmap) => ret = 0, // bump-куча не освобождает — утечка допустима (демо)
        Some(Lx::Mprotect) => {
            // Веха 108.4 — теперь по-настоящему: ld.so переводит RELRO в read-only и ставит
            // права сегментам, а страницы у нас появляются с правами из mmap.
            let (addr, len, prot) = (a0, a1, a2);
            lx_protect_range(t, cur, addr & !(PAGE - 1), (len + PAGE - 1) & !(PAGE - 1), prot);
            ret = 0;
        }
        Some(Lx::Madvise) => ret = 0,
        Some(Lx::Mremap) => ret = linux::err(linux::ENOSYS),
        // ── TLS и потоковые заглушки ────────────────────────────────────────────
        Some(Lx::ArchPrctl) => {
            // x86-64: ARCH_SET_FS(0x1002) — musl кладёт сюда базу TLS; ставим fsbase кадра.
            const ARCH_SET_FS: usize = 0x1002;
            if a0 == ARCH_SET_FS {
                t.procs[cur].frame.set_thread_ptr(a1);
                ret = 0;
            } else {
                ret = linux::err(linux::EINVAL);
            }
        }
        Some(Lx::Futex) => {
            // Веха 108.4 — futex(uaddr, op, val, …). glibc берёт его на КАЖДЫЙ внутренний
            // замок, и без него запуск обрывался на «The futex facility returned an unexpected
            // error code» — то есть на ENOSYS, а не на самой блокировке.
            //
            // Кладём на механизм Вехи 35, которым живут нити VOID: FUTEX_WAIT засыпает, если
            // слово ещё то самое, FUTEX_WAKE будит. Флаг PRIVATE (128) нам безразличен — futex
            // и так живёт в адресном пространстве процесса, а CLOCK_REALTIME (256) значим лишь
            // для таймаутов, которых мы пока не различаем.
            const FUTEX_WAIT: usize = 0;
            const FUTEX_WAKE: usize = 1;
            let (uaddr, op, val) = (a0, a1 & 0x7f, a2);
            match op {
                FUTEX_WAIT => {
                    let read = if ensure_heap_range(t, cur, uaddr, 4) {
                        Some(unsafe { core::ptr::read_volatile(uaddr as *const u32) })
                    } else {
                        None
                    };
                    match read {
                        Some(v) if v == val as u32 => {
                            // Ждём бессрочно: единственная нить linux-процесса разбудить себя не
                            // может, но и попасть сюда при своей же незанятой блокировке — тоже.
                            t.procs[cur].state = State::FutexWait;
                            t.procs[cur].futex_addr = uaddr;
                            t.procs[cur].futex_deadline = None;
                            if let Some(n) = t.next_runnable(cur) {
                                t.set_cur(n);
                            }
                            done = false; // спим: кадр и PC не трогаем (проснёмся — повторим)
                        }
                        // Значение уже иное — EAGAIN, как и положено futex'у.
                        _ => ret = linux::err(11),
                    }
                }
                FUTEX_WAKE => {
                    let space = t.procs[cur].space;
                    ret = wake_futex(t, space, uaddr, val);
                }
                _ => ret = linux::err(linux::ENOSYS),
            }
        }
        Some(Lx::SetTidAddress) => ret = cur + 1, // «tid» = индекс процесса + 1
        Some(Lx::SetRobustList) => ret = 0,
        Some(Lx::RtSigprocmask) => ret = 0,
        Some(Lx::RtSigaction) => ret = 0, // обработчики сигналов игнорируем (однопоточный CLI)
        Some(Lx::Rseq) => ret = linux::err(linux::ENOSYS),
        Some(Lx::Prlimit64) => ret = linux::err(linux::ENOSYS),
        // ── информация ──────────────────────────────────────────────────────────
        Some(Lx::Getpid) | Some(Lx::Gettid) => ret = cur + 1,
        Some(Lx::Getppid) => ret = 1,
        Some(Lx::Getuid) | Some(Lx::Geteuid) | Some(Lx::Getgid) | Some(Lx::Getegid) => ret = 0,
        // Мы «root» (uid/gid 0) — сброс привилегий busybox'а на старте no-op (успех).
        Some(Lx::Setuid) | Some(Lx::Setgid) | Some(Lx::Setgroups) => ret = 0,
        Some(Lx::Uname) => {
            let mut buf = [0u8; linux::UTSNAME_SIZE];
            linux::fill_utsname(&mut buf);
            ret = if lx_put(t, cur, a0, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Sysinfo) => {
            let zero = [0u8; 112]; // struct sysinfo — нулями (демо не читает поля критично)
            ret = if lx_put(t, cur, a0, &zero) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Getrandom) => {
            // Веха 86: был линейный конгруэнтный генератор от счётчика — теперь общий источник
            // ядра (аппаратный ГСЧ + пул событий, [`crate::random`]), тот же, что у SYS_RANDOM.
            let (buf, len) = (a0, a1);
            let mut tmp = alloc::vec![0u8; len];
            crate::random::fill(&mut tmp);
            ret = if lx_put(t, cur, buf, &tmp) { len } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Getcwd) => {
            // Корневой каталог: "/". Linux getcwd возвращает длину включая NUL.
            ret = if lx_put(t, cur, a0, b"/\0") { 2 } else { linux::err(linux::EFAULT) };
        }
        // ── время ────────────────────────────────────────────────────────────────
        // Веха 86: часы стали настоящими, поэтому REALTIME и MONOTONIC наконец РАЗНЫЕ.
        // clockid: 0 = CLOCK_REALTIME, 1 = CLOCK_MONOTONIC (прочие сводим к монотонному —
        // BOOTTIME/MONOTONIC_RAW у нас совпадают с ним, а CPU-таймеров процесса нет).
        Some(Lx::ClockGettime) => {
            let ns = match a0 {
                0 => crate::clock::realtime_ns(),
                _ => crate::clock::uptime_ns(),
            };
            let ts = [(ns / 1_000_000_000), (ns % 1_000_000_000)];
            let mut buf = [0u8; 16];
            buf[0..8].copy_from_slice(&ts[0].to_le_bytes());
            buf[8..16].copy_from_slice(&ts[1].to_le_bytes());
            ret = if lx_put(t, cur, a1, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Gettimeofday) => {
            // gettimeofday — всегда настенное время (у него нет clockid).
            let us = crate::clock::realtime_ns() / 1000;
            let tv = [(us / 1_000_000), (us % 1_000_000)];
            let mut buf = [0u8; 16];
            buf[0..8].copy_from_slice(&tv[0].to_le_bytes());
            buf[8..16].copy_from_slice(&tv[1].to_le_bytes());
            ret = if a0 == 0 || lx_put(t, cur, a0, &buf) { 0 } else { linux::err(linux::EFAULT) };
        }
        // Веха 114 — сон стал НАСТОЯЩИМ. Раньше обе эти заглушки возвращали 0 не поспав, и
        // программа, честно попросившая подождать, получала busy-loop: «спит» — а машина занята.
        // Теперь кладём на тот же механизм срока, что `futex_wait` и `SYS_SLEEP`.
        //
        // `nanosleep(req, rem)` и `clock_nanosleep(clockid, flags, req, rem)` — timespec из двух
        // 64-битных полей. Абсолютный срок (TIMER_ABSTIME=1) пересчитываем в относительный по
        // своим часам; `rem` не заполняем — просыпаться раньше срока у нас нечему (сигналов нет),
        // а значит остатка не бывает.
        Some(Lx::Nanosleep) | Some(Lx::ClockNanosleep) => {
            let a3 = t.procs[cur].frame.arg(3);
            let _ = a3; // `rem` не заполняем — просыпаться раньше срока у нас нечему
            let (req_va, abstime, clockid) = match decoded {
                Some(Lx::Nanosleep) => (a0, false, 1),
                _ => (a2, a1 & 1 != 0, a0),
            };
            match lx_get(t, cur, req_va, 16) {
                Some(ts) => {
                    let secs = u64::from_le_bytes(ts[0..8].try_into().unwrap());
                    let nsec = u64::from_le_bytes(ts[8..16].try_into().unwrap());
                    let want = secs.saturating_mul(1_000_000_000).saturating_add(nsec);
                    let ns = if abstime {
                        let now = if clockid == 0 {
                            crate::clock::realtime_ns()
                        } else {
                            crate::clock::uptime_ns()
                        };
                        want.saturating_sub(now)
                    } else {
                        want
                    };
                    if ns == 0 {
                        ret = 0;
                    } else {
                        let deadline =
                            arch::now_ticks().wrapping_add(crate::clock::ns_to_ticks(ns));
                        t.procs[cur].state = State::Sleeping;
                        t.procs[cur].futex_deadline = Some(deadline);
                        if let Some(n) = t.next_runnable(cur) {
                            t.set_cur(n);
                        }
                        done = false; // спим: кадр и PC не трогаем, возврат оформит пробуждение
                    }
                }
                None => ret = linux::err(linux::EFAULT),
            }
        }
        Some(Lx::SchedYield) => {
            ret = 0;
            // мягко уступить: пометим ret и дадим общему эпилогу продвинуть; переключение
            // сделает следующий тик — для CLI этого достаточно.
        }
        // ── файловые (пока без ФС: заглушки, что не роняют однопоточный CLI) ─────
        Some(Lx::Ioctl) => ret = linux::err(linux::ENOTTY), // isatty/TIOCGWINSZ → «не терминал»
        // Веха 186 — `fcntl`. Заглушка «всегда 0» была не мелочью, а ловушкой: `F_DUPFD` обязан
        // ВЕРНУТЬ НОМЕР, а ноль — законный номер, и оболочка, сохраняя им свой stdout, получала
        // в ответ `stdin`. Дальше она честно закрывала «сохранённое» и падала на `sh: 0: Bad file
        // descriptor`. Молчаливый успех дороже отказа ровно там, где успех что-то значит.
        Some(Lx::Fcntl) => {
            const F_DUPFD: usize = 0;
            const F_GETFD: usize = 1;
            const F_SETFD: usize = 2;
            const F_GETFL: usize = 3;
            const F_DUPFD_CLOEXEC: usize = 1030;
            const FD_CLOEXEC: usize = 1;
            let (fd, cmd, arg) = (a0, a1, a2);
            let known = t.procs[cur].lx_fds.get(fd).map(|s| s.is_some()).unwrap_or(false);
            ret = match cmd {
                F_DUPFD => lx_dup_fd(t, cur, fd, None, arg, false),
                F_DUPFD_CLOEXEC => lx_dup_fd(t, cur, fd, None, arg, true),
                _ if !known => linux::err(linux::EBADF),
                F_GETFD => {
                    let set = t.procs[cur].lx_fds[fd].as_ref().map(|f| f.cloexec).unwrap_or(false);
                    if set { FD_CLOEXEC } else { 0 }
                }
                F_SETFD => {
                    if let Some(Some(f)) = t.procs[cur].lx_fds.get_mut(fd) {
                        f.cloexec = arg & FD_CLOEXEC != 0;
                    }
                    0
                }
                // Режим открытия мы не храним: файл на запись узнаётся по буферу, всё
                // остальное читается. Отвечаем тем, что есть, а не выдуманным набором флагов.
                F_GETFL => {
                    let w = t.procs[cur].lx_fds[fd].as_ref().map(|f| f.wfile.is_some());
                    if w == Some(true) { 0o1 } else { 0 }
                }
                _ => 0,
            };
        }
        Some(Lx::Close) => {
            // fd 0/1/2 — «закрыты», реальных ресурсов нет; файловые — освободить слот.
            //
            // Веха 181 — и ЗДЕСЬ файл уезжает в store. Именно здесь, а не на каждом `write`:
            // объект неизменяем, значит всякая запись стоила бы нового объекта и нового корня, а
            // сборка пишет вывод компилятора байтами. `posixfs` поступает так же и по той же
            // причине.
            ret = 0;
            if a0 >= LX_FD_BASE {
                let taken = t.procs[cur].lx_fds.get_mut(a0 - LX_FD_BASE).and_then(|s| s.take());
                if let Some(sl) = taken {
                    if !lx_release_fd(t, cur, sl) {
                        ret = linux::err(linux::ENOSPC);
                    }
                }
            }
        }
        Some(Lx::Fork) => {
            // fork / vfork / clone(flags, stack, …).
            //
            // Нить (CLONE_VM — общее адресное пространство) мы НЕ делаем: у VOID для нитей есть
            // свой механизм, и подменять его линуксовым значило бы завести вторую модель
            // многопоточности в одной системе. Честный отказ лучше: `pthread_create` увидит
            // ENOSYS и скажет об этом, а не сломается посреди работы.
            const CLONE_VM: usize = 0x100;
            let flags = if decoded == Some(Lx::Fork) && (nr == 56 || nr == 220) { a0 } else { 0 };
            if flags & CLONE_VM != 0 {
                ret = linux::err(linux::ENOSYS);
            } else {
                let src = arch::space_root(t.procs[cur].space);
                match unsafe { arch::copy_user_space(src) } {
                    None => ret = linux::err(linux::ENOMEM),
                    Some(root) => {
                        // Имя ребёнка — имя родителя: ветвление не меняет программу.
                        let pname: &'static str = Box::leak(
                            alloc::string::String::from_utf8_lossy(
                                t.procs[cur].args.split(|&b| b == 0).next().unwrap_or(b"fork"),
                            )
                            .into_owned()
                            .into_boxed_str(),
                        );
                        let child = create_process_locked(t, pname, root, 0, 0);
                        // КАДР — копия родительского: ребёнок продолжает с той же строки, с тем
                        // же стеком и теми же регистрами. Этим `fork` и отличается от спавна:
                        // программа не начинается, а раздваивается.
                        t.procs[child].frame = t.procs[cur].frame;
                        t.procs[child].frame.set_ret(0); // ребёнку — ноль
                        t.procs[child].frame.advance();
                        t.procs[child].heap_brk = t.procs[cur].heap_brk;
                        t.procs[child].args = t.procs[cur].args.clone();
                        t.procs[child].env = t.procs[cur].env.clone();
                        t.procs[child].linux = true;
                        t.procs[child].image = t.procs[cur].image;
                        t.procs[child].parent = cur;
                        t.procs[child].zombie = true; // слот держим, пока родитель не заберёт код
                        // ДЕСКРИПТОРЫ достаются копией — так велит POSIX, и на этом стоит
                        // конвейер: `sh` открывает трубу ДО ветвления, а концы её разбирают уже
                        // потомки. У трубы от этого прибавляется держателей, и счётчик обязан об
                        // этом узнать — иначе закрытие одного конца объявило бы конец файла
                        // всем остальным.
                        t.procs[child].lx_fds = t.procs[cur].lx_fds.clone();
                        let inherited: Vec<(Option<(usize, bool)>, Option<usize>)> = t.procs
                            [child]
                            .lx_fds
                            .iter()
                            .flatten()
                            .map(|f| (f.pipe, f.wfile))
                            .collect();
                        for (pipe, wfile) in inherited {
                            if let Some((idx, write_end)) = pipe {
                                if let Some(Some(p)) = t.lx_pipes.get_mut(idx) {
                                    if write_end {
                                        p.writers += 1;
                                    } else {
                                        p.readers += 1;
                                    }
                                }
                            }
                            // Файл на запись тоже наследуется держателем: иначе `close` у
                            // ребёнка сбросил бы содержимое в store, пока родитель ещё пишет.
                            if let Some(wi) = wfile {
                                if let Some(Some(w)) = t.lx_wfiles.get_mut(wi) {
                                    w.holders += 1;
                                }
                            }
                        }
                        // Права — копиями, как при спавне (`cap::endow`), и с тем же уважением к
                        // пометке «не наследуется»: ветвление не должно быть лазейкой, через
                        // которую утекает то, что родитель держит при себе.
                        let dom = t.procs[cur].domain;
                        let cdom = t.procs[child].domain;
                        for bits in t.procs[cur].start_caps.clone() {
                            let pc = Cap::from_bits(bits as u64);
                            if !cap::inheritable(dom, pc) {
                                continue;
                            }
                            if let Ok(c) = cap::endow(dom, pc, cdom) {
                                t.procs[child].start_caps.push(c.bits() as usize);
                            }
                        }
                        vprintln!("  [linux] P{} fork → P{}", cur, child);
                        ret = child; // родителю — номер ребёнка
                    }
                }
            }
        }
        Some(Lx::Wait4) => {
            // wait4(pid, wstatus, options, rusage): pid -1 — любой ребёнок, >0 — этот.
            // `rusage` не заполняем: счётчиков на процесс у нас нет, и врать нулями хуже, чем
            // не трогать (вызывающий обычно передаёт NULL).
            const WNOHANG: usize = 1;
            let (want, status_va, options) = (a0 as isize, a1, a2);
            let mut any_child = false;
            let mut ready: Option<usize> = None;
            for i in 0..t.procs.len() {
                if t.procs[i].parent != cur || !t.procs[i].zombie {
                    continue;
                }
                if want > 0 && want as usize != i {
                    continue;
                }
                any_child = true;
                if t.procs[i].exit_code.is_some() {
                    ready = Some(i);
                    break;
                }
            }
            ret = match ready {
                Some(pid) => {
                    let code = t.procs[pid].exit_code.unwrap_or(0);
                    // Слово состояния Linux: у нормально завершившегося это код в байтах 8..16.
                    // Убитых сигналом у нас не бывает — сигналов нет вовсе.
                    let status = ((code as u32 & 0xff) << 8).to_le_bytes();
                    if status_va != 0 && !lx_put(t, cur, status_va, &status) {
                        linux::err(linux::EFAULT)
                    } else {
                        // Похоронить: зомби своё отслужил, слот вернуть.
                        t.procs[pid].zombie = false;
                        if t.procs[pid].state == State::Finished && pid != t.cur() {
                            t.free_slots.push(pid);
                        }
                        vprintln!("  [linux] P{} wait4 → P{} код {}", cur, pid, code);
                        pid
                    }
                }
                // Детей нет вовсе — ждать нечего, и сказать это надо отдельным кодом: иначе
                // оболочка, ждущая в цикле, крутилась бы вечно.
                None if !any_child => linux::err(linux::ECHILD),
                None if options & WNOHANG != 0 => 0,
                None => {
                    // Дети есть, но никто не закончил. Ждём: проснувшись, вызов повторится
                    // сам (PC стоит на `syscall`) и сам найдёт зомби.
                    let key = if want > 0 { want as usize } else { ANY_CHILD };
                    t.procs[cur].state = State::ChildWait(key);
                    if let Some(nx) = t.next_runnable(cur) {
                        t.set_cur(nx);
                    }
                    done = false;
                    0
                }
            };
        }
        Some(Lx::Pipe2) => {
            // pipe2(fds[2], flags) / legacy pipe(fds[2]). `O_CLOEXEC` соблюдается (Веха 186:
            // после `fork` лишний держатель конца трубы — это зависший конвейер), `O_NONBLOCK`
            // молча игнорируется: он соврал бы — мы всегда блокируемся.
            let flags = if decoded == Some(Lx::Pipe2) { a1 } else { 0 };
            let idx = pipe_alloc(t);
            let mk = |t: &mut Table, write_end: bool| -> usize {
                let fd = LxFd {
                    meta: crate::lxfs::Meta {
                        id: void_abi::ContentId([0u8; 32]),
                        size: 0,
                        ty: void_tree::K_FILE,
                        src: crate::lxfs::Src::Hier,
                    },
                    off: 0,
                    path: Vec::new(),
                    dpos: 0,
                    wfile: None,
                    pipe: Some((idx, write_end)),
                    console: false,
                    cloexec: flags & O_CLOEXEC != 0,
                };
                let tbl = &mut t.procs[cur].lx_fds;
                match tbl.iter().position(|s| s.is_none()) {
                    Some(i) => {
                        tbl[i] = Some(fd);
                        i + LX_FD_BASE
                    }
                    None => {
                        tbl.push(Some(fd));
                        tbl.len() - 1 + LX_FD_BASE
                    }
                }
            };
            let rfd = mk(t, false);
            let wfd = mk(t, true);
            let mut out = [0u8; 8];
            out[0..4].copy_from_slice(&(rfd as u32).to_le_bytes());
            out[4..8].copy_from_slice(&(wfd as u32).to_le_bytes());
            ret = if lx_put(t, cur, a0, &out) {
                vprintln!("  [linux] P{} pipe2 → fd {} и {}", cur, rfd, wfd);
                0
            } else {
                // Не смогли отдать номера — труба никому не досталась, свернуть её целиком.
                pipe_close_end(t, idx, false);
                pipe_close_end(t, idx, true);
                linux::err(linux::EFAULT)
            };
        }
        Some(Lx::Execve) => {
            // execve(path, argv[], envp[]) — массивы указателей, каждый кончается NULL.
            let (path_va, argv_va, envp_va) = (a0, a1, a2);
            // Собрать блоб NUL-разделённых строк из массива указателей. Потолок — тот же
            // `ARGS_MAX`, что у своих процессов: чужой массив без NULL иначе увёл бы ядро в
            // бесконечное чтение.
            let gather = |t: &mut Table, base: usize| -> Option<Vec<u8>> {
                let mut out: Vec<u8> = Vec::new();
                if base == 0 {
                    return Some(out);
                }
                for i in 0..256usize {
                    let ent = lx_get(t, cur, base + i * 8, 8)?;
                    let ptr = usize::from_le_bytes(ent.try_into().ok()?);
                    if ptr == 0 {
                        return Some(out);
                    }
                    let sarg = lx_cstr(t, cur, ptr, 4096)?;
                    if out.len() + sarg.len() + 1 > ARGS_MAX {
                        return None;
                    }
                    out.extend_from_slice(&sarg);
                    out.push(0);
                }
                None // NULL так и не встретился — считаем массив негодным
            };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) => match crate::lxfs::lookup(&path) {
                    None => linux::err(linux::ENOENT),
                    Some(meta) => match crate::lxfs::read_all(&meta) {
                        None => linux::err(linux::ENOENT),
                        Some(image) => {
                            match (gather(t, argv_va), gather(t, envp_va)) {
                                (Some(argv), Some(envp)) => {
                                    // Имя процесса обязано жить дольше таблицы (как и при
                                    // спавне): утечка на запуск, запусков за сессию немного.
                                    let pname: &'static str = alloc::boxed::Box::leak(
                                        alloc::string::String::from_utf8_lossy(&path)
                                            .into_owned()
                                            .into_boxed_str(),
                                    );
                                    if exec_linux_in_place(t, cur, pname, &image, argv, envp) {
                                        vprintln!("  [linux] P{} execve → {}", cur, pname);
                                        // Кадр уже НОВЫЙ — общий эпилог его трогать не должен.
                                        done = false;
                                        0
                                    } else {
                                        linux::err(linux::ENOMEM)
                                    }
                                }
                                _ => linux::err(linux::EFAULT),
                            }
                        }
                    },
                },
            };
        }
        // ── создание и снятие имён (Веха 181, ADR 0019) ──────────────────────────
        //
        // Путь берётся ВСЕГДА абсолютным: `cwd` Linux-процесса у нас `/`, и относительных путей
        // нет — это записано отдельным пунктом в известных пробелах, а не забыто. `dirfd`
        // поэтому игнорируется; когда появится `chdir`, разбор придёт сюда же.
        Some(Lx::Mkdirat) => {
            // legacy `mkdir(path, mode)` кладёт путь первым аргументом, `mkdirat` — вторым.
            let legacy = nr == 83;
            let path_va = if legacy { a0 } else { a1 };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) if crate::lxfs::lookup_nofollow(&path).is_some() => {
                    linux::err(linux::EEXIST)
                }
                Some(path) if crate::lxfs::mkdir(&path) => 0,
                Some(_) => linux::err(linux::ENOENT),
            };
        }
        Some(Lx::Unlinkat) => {
            // legacy `unlink(path)`/`rmdir(path)` — путь первым; `unlinkat(dirfd, path, flags)`
            // — вторым. Флаг `AT_REMOVEDIR` нам не нужен: что это каталог, мы и так видим.
            let legacy = nr == 87 || nr == 84;
            let path_va = if legacy { a0 } else { a1 };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) if crate::lxfs::lookup_nofollow(&path).is_none() => {
                    linux::err(linux::ENOENT)
                }
                Some(path) if crate::lxfs::unlink(&path) => 0,
                // Не вышло, а путь есть — значит каталог с содержимым. Рекурсии здесь нет
                // намеренно: снести дерево одним вызовом ядро не станет.
                Some(_) => linux::err(linux::ENOTEMPTY),
            };
        }
        Some(Lx::Renameat) => {
            // legacy `rename(old, new)` — оба пути первыми; `renameat(olddirfd, old, newdirfd,
            // new)` — вторым и четвёртым; `renameat2` добавляет флаги, которых мы не умеем.
            let legacy = nr == 82;
            let (o_va, n_va) = if legacy { (a0, a1) } else { (a1, a(t, 3)) };
            ret = match (lx_cstr(t, cur, o_va, 4096), lx_cstr(t, cur, n_va, 4096)) {
                (Some(o), Some(n)) if crate::lxfs::rename(&o, &n) => 0,
                (Some(o), Some(_)) if crate::lxfs::lookup_nofollow(&o).is_none() => {
                    linux::err(linux::ENOENT)
                }
                // Каталоги ядро не переименовывает: путь входит в имя корня каждого потомка, то
                // есть это обход поддерева, и он уже написан в `posixfs`.
                (Some(_), Some(_)) => linux::err(linux::EINVAL),
                _ => linux::err(linux::EFAULT),
            };
        }
        // ── файлы (Веха 108.3): читаются ПРЯМО ИЗ STORE, см. [`crate::lxfs`] ──────
        Some(Lx::Openat) | Some(Lx::Open) => {
            // openat(dirfd, path, flags, mode) либо legacy open(path, flags, mode) — разница
            // только в том, где лежит путь. Относительных путей у нас нет: cwd Linux-процесса
            // всегда `/`, а пакеты адресуются абсолютно — этого хватает и ld.so, и applet'ам.
            const O_WRONLY: usize = 0o1;
            const O_RDWR: usize = 0o2;
            const O_CREAT: usize = 0o100;
            let legacy = decoded == Some(Lx::Open);
            let (path_va, flags) = if legacy { (a0, a1) } else { (a1, a2) };
            const O_TRUNC: usize = 0o1000;
            const O_APPEND: usize = 0o2000;
            let writing = flags & (O_WRONLY | O_RDWR | O_CREAT) != 0;
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                // Веха 181 — ЗАПИСЬ. Дерево пакета остаётся неизменяемым: это не недоделка, а
                // его смысл, и отказ здесь честнее молчаливого успеха.
                Some(path) if writing && crate::lxfs::in_package(&path) => {
                    vprintln!("  [linux] P{} openat на запись в пакет — EROFS", cur);
                    linux::err(linux::EROFS)
                }
                // Веха 190 — каталога нет, значит и файла быть не может. Раньше открытие
                // молчаливо удавалось, а пропажа обнаруживалась на `close` — то есть никогда:
                // умирающий процесс закрывает дескрипторы сам и на отказ не смотрит. Сборка от
                // этого «проходила», не написав ни байта.
                Some(path) if writing && !crate::lxfs::parent_dir_exists(&path) => {
                    vprintln!("  [linux] P{} openat на запись: нет каталога у пути", cur);
                    linux::err(linux::ENOENT)
                }
                Some(path) if writing => {
                    let existing = crate::lxfs::lookup(&path);
                    // Содержимое, с которого начинаем: пусто при O_TRUNC и у нового файла,
                    // прежнее при O_APPEND. Дописывать в объект store нельзя — он неизменяем,
                    // поэтому файл собирается в буфере целиком и кладётся на `close`.
                    let start = match &existing {
                        Some(m) if flags & O_TRUNC == 0 && flags & O_APPEND != 0 => {
                            crate::lxfs::read_all(m).unwrap_or_default()
                        }
                        _ => Vec::new(),
                    };
                    let meta = existing.unwrap_or(crate::lxfs::Meta {
                        id: void_abi::ContentId([0u8; 32]),
                        size: 0,
                        ty: void_tree::K_FILE,
                        src: crate::lxfs::Src::Hier,
                    });
                    let off = start.len() as u64;
                    let fd = lx_fd_alloc(t, cur, meta, path.clone());
                    let w = LxWrite { path, buf: start, holders: 1 };
                    let wi = match t.lx_wfiles.iter().position(|s| s.is_none()) {
                        Some(i) => {
                            t.lx_wfiles[i] = Some(w);
                            i
                        }
                        None => {
                            t.lx_wfiles.push(Some(w));
                            t.lx_wfiles.len() - 1
                        }
                    };
                    if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                        sl.wfile = Some(wi);
                        sl.off = off;
                    }
                    if flags & O_CLOEXEC != 0 {
                        if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                            sl.cloexec = true;
                        }
                    }
                    vprintln!("  [linux] P{} openat на запись → fd {}", cur, fd);
                    fd
                }
                Some(path) => match crate::lxfs::lookup(&path) {
                    Some(meta) => {
                        let fd = lx_fd_alloc(t, cur, meta, path);
                        if flags & O_CLOEXEC != 0 {
                            if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                                sl.cloexec = true;
                            }
                        }
                        vprintln!("  [linux] P{} openat → fd {}", cur, fd);
                        fd
                    }
                    None => linux::err(linux::ENOENT),
                },
            };
        }
        Some(Lx::Faccessat) | Some(Lx::Access) => {
            let path_va = if decoded == Some(Lx::Access) { a0 } else { a1 };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                Some(path) if crate::lxfs::lookup(&path).is_some() => 0,
                Some(_) => linux::err(linux::ENOENT),
                None => linux::err(linux::EFAULT),
            };
        }
        Some(Lx::Readlinkat) | Some(Lx::Readlink) => {
            // legacy readlink(path, buf, size) — путь в ПЕРВОМ аргументе, dirfd'а нет.
            let (path_va, buf, len) = if decoded == Some(Lx::Readlink) {
                (a0, a1, a2)
            } else {
                (a1, a2, a(t, 3))
            };
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(path) => match crate::lxfs::lookup(&path).and_then(|m| crate::lxfs::readlink(&m))
                {
                    Some(target) => {
                        let n = target.len().min(len);
                        // readlink НЕ дописывает нуль — так в Linux, и musl на это рассчитывает.
                        if lx_put(t, cur, buf, &target[..n]) {
                            n
                        } else {
                            linux::err(linux::EFAULT)
                        }
                    }
                    None => linux::err(linux::EINVAL), // не ссылка либо нет пути
                },
            };
        }
        Some(Lx::Getdents64) => {
            // Записи каталога в linux-формате: d_ino(8) d_off(8) d_reclen(2) d_type(1) имя+NUL.
            let (fd, buf, len) = (a0, a1, a2);
            let slot = fd.checked_sub(LX_FD_BASE).and_then(|i| t.procs[cur].lx_fds.get(i).cloned());
            match slot.flatten() {
                None => ret = linux::err(linux::EBADF),
                Some(f) => {
                    let entries = crate::lxfs::dir_entries(&f.path, &f.meta);
                    let mut out: Vec<u8> = Vec::new();
                    let mut pos = f.dpos;
                    while pos < entries.len() {
                        let (ty, name) = &entries[pos];
                        let reclen = (19 + name.len() + 1 + 7) & !7; // выравнивание на 8
                        if out.len() + reclen > len {
                            break;
                        }
                        // d_ino/d_off — не несут смысла в content-адресуемом сторе; ставим номер
                        // записи: musl требует лишь монотонности и ненулевого d_ino.
                        out.extend_from_slice(&((pos as u64) + 1).to_le_bytes());
                        out.extend_from_slice(&((pos as u64) + 1).to_le_bytes());
                        out.extend_from_slice(&(reclen as u16).to_le_bytes());
                        out.push(if void_tree::is_dir(*ty) {
                            4 // DT_DIR
                        } else if void_tree::is_link(*ty) {
                            10 // DT_LNK
                        } else {
                            8 // DT_REG
                        });
                        out.extend_from_slice(name.as_bytes());
                        out.push(0);
                        while out.len() % 8 != 0 {
                            out.push(0);
                        }
                        pos += 1;
                    }
                    if let Some(Some(sl)) = t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE) {
                        sl.dpos = pos;
                    }
                    ret = if lx_put(t, cur, buf, &out) {
                        out.len()
                    } else {
                        linux::err(linux::EFAULT)
                    };
                }
            }
        }
        Some(Lx::Lseek) => {
            let (fd, off, whence) = (a0, a1 as i64, a2);
            if !matches!(fd_kind(t, cur, fd), FdKind::File) {
                // Ни консоль, ни труба не позиционируются — это поток, а не файл.
                ret = linux::err(linux::ESPIPE);
            } else {
                match t.procs[cur].lx_fds.get_mut(fd - LX_FD_BASE).and_then(|s| s.as_mut()) {
                    None => ret = linux::err(linux::EBADF),
                    Some(f) => {
                        let base = match whence {
                            1 => f.off as i64,
                            2 => f.meta.size as i64,
                            _ => 0,
                        };
                        let p = (base + off).clamp(0, f.meta.size as i64) as u64;
                        f.off = p;
                        ret = p as usize;
                    }
                }
            }
        }
        // ── дублирование дескрипторов (Веха 186) ─────────────────────────────────
        //
        // Ради этого консоль и стала обычной записью таблицы: `dup2` — это ПРИСВАИВАНИЕ, и
        // присваивать надо было нечему, пока 0/1/2 существовали лишь как условие `if fd == 1`.
        // Оболочка строит перенаправление и конвейер ровно им: заводит трубу, ветвится и в
        // ребёнке кладёт её конец на место `stdout`.
        Some(Lx::Dup) | Some(Lx::Dup3) => {
            // dup(old) — свободный номер; dup2(old,new)/dup3(old,new,flags) — названный.
            let named = decoded == Some(Lx::Dup3);
            let to = named.then_some(a1);
            // `dup3` умеет сразу пометить копию `CLOEXEC`; у `dup`/`dup2` копия помечена быть не
            // может — так велит POSIX, и на этом стоит перенаправление (иначе `dup2(труба, 1)`
            // отдал бы образу stdout, который сам же и закрыл бы на входе).
            let cloexec = named && nr != 33 && a2 & O_CLOEXEC != 0;
            ret = lx_dup_fd(t, cur, a0, to, 0, cloexec);
        }
        // Веха 190 — `umask`. Прав у файлов в VOID нет по замыслу ([[no-users-root]]), поэтому
        // маска не значит ничего. Но отвечать отказом нельзя: POSIX не позволяет этому вызову
        // падать, и звонящий разбирает ответ как ПРЕЖНЮЮ маску. `busybox mkdir` считает по ней
        // режим создаваемого каталога и на `-ENOSYS` уезжает в бессмыслицу.
        Some(Lx::Umask) => ret = 0o022,
        Some(Lx::Ppoll) => ret = linux::err(linux::ENOSYS),
        // Веха 186 — `fstat` КОНСОЛИ: у неё нет узла в store, и врать про файл нельзя. Отвечаем
        // символьным устройством — тем, чем консоль и является; `isatty` из musl спрашивает
        // именно это.
        Some(Lx::Fstat) if fd_kind(t, cur, a0) == FdKind::Console => {
            let mut st = alloc::vec![0u8; linux::STAT_SIZE];
            linux::fill_stat_chr(&mut st);
            ret = if lx_put(t, cur, a1, &st) { 0 } else { linux::err(linux::EFAULT) };
        }
        Some(Lx::Fstat) if fd_kind(t, cur, a0) == FdKind::File => {
            // fstat файла из store: тип и размер знает узел дерева.
            let (fd, buf) = (a0, a1);
            let slot = t.procs[cur].lx_fds.get(fd - LX_FD_BASE).cloned().flatten();
            ret = match slot {
                None => linux::err(linux::EBADF),
                Some(f) => {
                    let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                    linux::fill_stat_file(&mut st, f.meta.size, f.meta.ty, crate::lxfs::ino(&f.meta));
                    if lx_put(t, cur, buf, &st) {
                        0
                    } else {
                        linux::err(linux::EFAULT)
                    }
                }
            };
        }
        Some(Lx::Fstat) => {
            // fstat(fd, buf): только символьные устройства stdin/out/err (fd 0/1/2).
            let (fd, buf) = (a0 as isize, a1);
            if (0..=2).contains(&fd) {
                let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                linux::fill_stat_chr(&mut st);
                ret = if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) };
            } else {
                ret = linux::err(linux::EBADF);
            }
        }
        Some(Lx::Stat) | Some(Lx::Lstat) => {
            // legacy stat(path, buf) / lstat(path, buf). Разницы между ними у нас нет: путь мы и
            // так не разыменовываем, а тип записи отдаём настоящий (см. долги вехи).
            let (path_va, buf) = (a0, a1);
            ret = match lx_cstr(t, cur, path_va, 4096) {
                None => linux::err(linux::EFAULT),
                Some(p) => match crate::lxfs::lookup(&p) {
                    Some(meta) => {
                        let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                        linux::fill_stat_file(&mut st, meta.size, meta.ty, crate::lxfs::ino(&meta));
                        if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                    }
                    None => linux::err(linux::ENOENT),
                },
            };
        }
        Some(Lx::Newfstatat) => {
            // newfstatat(dirfd, path, buf, flags): без пути (AT_EMPTY_PATH) и fd 0/1/2 —
            // символьное устройство; с путём — ФС ещё нет, значит файла нет (ENOENT).
            const AT_EMPTY_PATH: usize = 0x1000;
            let (dirfd, path, buf, flags) = (a0 as isize, a1, a2, a(t, 3));
            let empty_path = flags & AT_EMPTY_PATH != 0
                || lx_get(t, cur, path, 1).map_or(true, |b| b.first() == Some(&0));
            if empty_path && (0..=2).contains(&dirfd) {
                let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                linux::fill_stat_chr(&mut st);
                ret = if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) };
            } else if empty_path && dirfd >= LX_FD_BASE as isize {
                // AT_EMPTY_PATH на открытом файле = fstat.
                let slot = t.procs[cur].lx_fds.get(dirfd as usize - LX_FD_BASE).cloned().flatten();
                ret = match slot {
                    None => linux::err(linux::EBADF),
                    Some(f) => {
                        let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                        linux::fill_stat_file(&mut st, f.meta.size, f.meta.ty, crate::lxfs::ino(&f.meta));
                        if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                    }
                };
            } else {
                // Веха 108.3 — путь ищется в store. AT_SYMLINK_NOFOLLOW нам безразличен: путь
                // и так не разыменовывается (см. долги вехи), а тип записи мы отдаём настоящий.
                ret = match lx_cstr(t, cur, path, 4096) {
                    None => linux::err(linux::EFAULT),
                    Some(p) => match crate::lxfs::lookup(&p) {
                        Some(meta) => {
                            let mut st = alloc::vec![0u8; linux::STAT_SIZE];
                            linux::fill_stat_file(&mut st, meta.size, meta.ty, crate::lxfs::ino(&meta));
                            if lx_put(t, cur, buf, &st) { 0 } else { linux::err(linux::EFAULT) }
                        }
                        None => linux::err(linux::ENOENT),
                    },
                };
            }
        }
        // ── завершение ────────────────────────────────────────────────────────────
        Some(Lx::Exit) | Some(Lx::ExitGroup) => {
            let code = a0 & 0xff;
            let leader = t.procs[cur].group;
            vprintln!("  [linux] P{} exit_group({}) — процесс P{}", cur, code, leader);
            for i in 0..t.procs.len() {
                if t.procs[i].group == leader {
                    t.procs[i].state = State::Finished;
                    lx_close_all(t, i);
                }
            }
            crate::net::ext_detach(leader); // Веха 195: карта ушла с процессом
            wake_exec_waiters(t, leader, code);
            if let Some(n) = t.next_runnable(cur) {
                t.set_cur(n);
            }
            done = false; // процесс завершён — PC/кадр не трогаем
        }
        None => {
            vprintln!("  [linux] P{} НЕреализованный syscall #{} — ENOSYS", cur, nr);
            ret = linux::err(linux::ENOSYS);
        }
    }

    if done {
        let f = &mut t.procs[cur].frame;
        f.set_ret(ret);
        f.skip_syscall_insn(); // riscv: sepc+4; x86: rip+2 (пройти `syscall`)
    }
}

// ─── что персоналия держит В ТАБЛИЦЕ ПРОЦЕССОВ (Веха 214.2) ─────────────────
//
// Поля `Proc`/`Table` описаны там же, где таблица, — а их ТИПЫ живут здесь: дескрипторы, трубы
// и открытые на запись файлы нужны только персоналии Linux, и родной части ядра о них знать
// нечего. Потомок виден предку по имени модуля, поэтому граница не размылась: `Table` хранит
// `lxabi::LxPipe`, а не «что-то из соседнего файла».

/// Открытый файл личности Linux: что читать, откуда и где мы в нём находимся.
#[derive(Clone)]
pub(super) struct LxFd {
    meta: crate::lxfs::Meta,
    /// Позиция чтения (`lseek`/`read`).
    off: u64,
    /// Путь — нужен `getdents64` (перечисление `/nix/store` идёт по корням, а не по узлу) и
    /// диагностике.
    path: Vec<u8>,
    /// Сколько записей каталога уже отдано `getdents64`.
    dpos: usize,
    /// Веха 186 — это КОНСОЛЬ (`stdin`/`stdout`/`stderr`). Отдельным полем, а не особым
    /// случаем в коде: пока 0/1/2 были не записями таблицы, а условием `if fd == 1`, подменить
    /// их было нечем — а именно этим и живут перенаправление и конвейер. Настоящий `sh`
    /// упирался в это как `dup2(0,1): Function not implemented`.
    console: bool,
    /// Веха 186 — «не переживать `execve`» (`FD_CLOEXEC`). Оболочка помечает так СВОИ рабочие
    /// дескрипторы — сохранённый stdout, концы трубы, — и рассчитывает, что образ, пришедший на
    /// смену, их не унаследует. Не выполнить эту пометку — значит оставить лишнего держателя у
    /// трубы: читатель ждал бы конца файла, который никто не объявит.
    cloexec: bool,
    /// Веха 183 — это дескриптор ТРУБЫ: её номер и с какого она конца (`true` — пишущий).
    /// Труба живёт в таблице, а не в дескрипторе, потому что концов у неё двое, а после
    /// `fork` станет и вчетверо больше: владелец у неё не один.
    pipe: Option<(usize, bool)>,
    /// Веха 181 — файл открыт НА ЗАПИСЬ: номер записи в [`Table::lx_wfiles`]. Содержимое
    /// собирается там и уезжает в store, когда уходит последний держатель — объект в store
    /// неизменяем, дописать в него нельзя, можно лишь положить новый целиком.
    wfile: Option<usize>,
}

/// Веха 183 — ТРУБА: байты, ждущие читателя, и сколько концов ещё открыто.
///
/// Ёмкость конечная и это важно: без неё писатель, который быстрее читателя, съел бы всю память
/// ядра. Упёрся в потолок — ждёт, ровно как на Linux.
pub(super) struct LxPipe {
    buf: alloc::collections::VecDeque<u8>,
    readers: usize,
    writers: usize,
}

/// Сколько байт труба держит, пока их не забрали. Столько же по умолчанию у Linux.
const PIPE_CAP: usize = 64 * 1024;

/// Веха 186 — ФАЙЛ, ОТКРЫТЫЙ НА ЗАПИСЬ. В таблице, а не в дескрипторе, по той же причине, что и
/// труба: держателей у него бывает несколько.
///
/// Так выглядит перенаправление: оболочка открывает файл, делает `dup2(fd, 1)` — и теперь на
/// него смотрят ДВА дескриптора. Пока буфер лежал в дескрипторе, копия несла бы вторую правду о
/// содержимом, и чей `close` последним, того и файл. Поэтому буфер один, а дескрипторы лишь
/// считаются держателями; в store содержимое уезжает, когда уходит последний.
pub(super) struct LxWrite {
    path: Vec<u8>,
    buf: Vec<u8>,
    holders: usize,
}


/// Веха 38 — завести LINUX-процесс из static-PIE ELF (ET_DYN) под УЖЕ взятым замком таблицы.
/// В отличие от [`spawn_elf`] (наш ET_EXEC): образ грузится по базе [`USER_REGION_START`]
/// ([`elf::load_pie`], musl само-релоцируется), а вместо регистров-аргументов строится
/// стартовый стек Linux — `argc/argv/envp/`**`auxv`** ([`crate::linux::build_init_stack`]),
/// по которому musl находит себя, канарейку и (для многопоточных) TLS. Процесс помечается
/// `linux` — его syscall'ы поедут в трансля́тор [`crate::linux`]. `root` — уже созданное
/// адресное пространство (клон ядра + стек). `None` — негодный образ или нет памяти.
pub(super) fn spawn_linux_locked(
    t: &mut Table,
    pname: &'static str,
    bytes: &[u8],
    root: usize,
    args_blob: Vec<u8>,
    parent_env: &[u8],
) -> Option<usize> {
    let pie = match elf::load_pie(root, bytes, USER_REGION_START, USER_HEAP_BASE_VA) {
        Ok(p) => p,
        Err(e) => {
            vprintln!("  [linux] негодный PIE-образ: {:?}", e);
            return None;
        }
    };

    // Веха 108.4 — ДИНАМИЧЕСКИЙ бинарь: у него в `PT_INTERP` записан абсолютный путь загрузчика
    // (`/nix/store/…/ld-linux-…so`), то есть ровно в тот пакет, который мы уже умеем прочитать.
    // Грузим загрузчик вторым образом и передаём управление ЕМУ: релокации, поиск библиотек и
    // их отображение он делает сам — ядру остаётся дать ему mmap файлов и верный auxv.
    let mut interp_base = 0usize;
    let mut entry = pie.entry;
    if let Some(ipath) = elf::interp_path(bytes) {
        let Some(meta) = crate::lxfs::lookup(ipath) else {
            println!("  [linux] загрузчик не найден: {}", core::str::from_utf8(ipath).unwrap_or("?"));
            return None;
        };
        let Some(idata) = crate::lxfs::read_all(&meta) else {
            println!("  [linux] загрузчик не читается");
            return None;
        };
        match elf::load_pie(root, &idata, INTERP_BASE_VA, USER_HEAP_BASE_VA) {
            Ok(ip) => {
                interp_base = INTERP_BASE_VA;
                entry = ip.entry;
                vprintln!(
                    "  [linux] загрузчик {} → база {:#x}, вход {:#x}",
                    core::str::from_utf8(ipath).unwrap_or("?"),
                    INTERP_BASE_VA,
                    entry
                );
            }
            Err(e) => {
                println!("  [linux] загрузчик негоден: {:?}", e);
                return None;
            }
        }
    }
    // Окружение НАСЛЕДУЕТСЯ от запустившего — как у родных процессов VOID (Веха 187). Раньше
    // оно было прибито гвоздями, и это держалось ровно до первой сборки: деривация ЕСТЬ
    // окружение (`$out`, `$name`, флаги компилятора), и передать его было нечем.
    //
    // Умолчания остаются, но только для того, чего родитель не назвал: у шелла VOID нет ни
    // `PATH`, ни `HOME`, и без них чужой `sh` не найдёт даже самого себя. Названное родителем
    // не трогаем — иначе песочница не смогла бы задать `PATH=/path-not-set`, а без него сборка
    // тихо разъезжается от машины к машине.
    let mut env: Vec<u8> = Vec::from(parent_env);
    if !env.is_empty() && *env.last().unwrap() != 0 {
        env.push(0);
    }
    for (key, line) in [
        (&b"PATH="[..], &b"PATH=/bin:/usr/bin\0"[..]),
        (&b"TERM="[..], &b"TERM=linux\0"[..]),
        (&b"HOME="[..], &b"HOME=/\0"[..]),
    ] {
        if !env.split(|&b| b == 0).any(|rec| rec.starts_with(key)) {
            env.extend_from_slice(line);
        }
    }
    let env: &[u8] = &env;
    // 16 байт AT_RANDOM (канарейка/ГПСЧ musl) — из счётчика тиков через splitmix64.
    let mut rnd = [0u8; 16];
    let mut seed = arch::now_ticks();
    for chunk in rnd.chunks_mut(8) {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = seed.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    let (block, sp) =
        crate::linux::build_init_stack(USER_STACK_TOP_VA, &args_blob, env, &pie, rnd, interp_base);

    let child = create_process_locked(t, pname, root, entry, 0);
    // Стартовый кадр: вход = pie.entry, sp = вершина построенного стека (там argc). Аргументы
    // Linux читает со стека, а не из регистров — a0/rdi обнулены (musl `_start` их игнорирует).
    t.procs[child].frame = TrapFrame::new_user(entry, sp, 0);
    t.procs[child].linux = true;
    lx_init_stdio(t, child);
    t.procs[child].args = args_blob;
    t.procs[child].env = Vec::from(env);
    copy_to_space(arch::space_root(t.procs[child].space), sp, &block);
    Some(child)
}
