//! Веха 214.3 — **память процесса**: адресное пространство, ленивая куча, общие страницы.
//!
//! Собрано из кусков, лежавших по всему `proc.rs` и связанных одним: все они отвечают на вопрос
//! «что процесс видит по этому адресу и чем за это заплачено».
//!
//! - **пространство** — завести, сбросить буфер трансляций, съехать, вернуть фреймы мёртвых;
//! - **ленивая куча** — отказ страницы как обычный ход событий, а не беда ([`handle_user_fault`]),
//!   и доотображение диапазона перед чтением его ЯДРОМ ([`ensure_heap_range`]);
//! - **общие страницы** — завести область, отобразить чужую, снять ([`shm_map_new`] и соседи);
//! - **перенос байтов** между пространствами — ядро ходит по ним через direct-map, а не
//!   переключая корень.
//!
//! **Почему это один модуль, а не четыре.** Порознь они выглядят разными темами, но держатся на
//! одном инварианте: страницу освобождает ТОТ, ЧЬЯ ОНА. Общая — область по счётчику держателей,
//! DMA-страница — никто (в неё пишет железо, Веха 213.2), MMIO — никто (это не RAM), приватная —
//! умирающий процесс. Стоит развести эти места по модулям, и правило снова придётся помнить
//! вместо того, чтобы видеть.
//!
//! Подмодуль `proc`, а не сосед, по той же причине, что и [`super::lxabi`]: всё здесь работает
//! приватными полями `Table` и `Proc`, и выносить их наружу ради расположения файлов нечестно.

use super::*;

/// Новое адресное пространство процесса: клон корня ядра (ядро отображено без флага U — нужно
/// trap-обработчику при satp процесса) + приватный стек в незанятом регионе VPN[2]=1. Код и
/// данные добавит [`crate::elf::load`]: с Вехи 23 процессы приходят ТОЛЬКО из ELF в store.
/// Веха 89 — `None`, если памяти не хватило. Раньше здесь стояла паника: программа, которую
/// нечем запустить, роняла ЯДРО. Частично построенное пространство сносим целиком, чтобы фреймы
/// не утекли (`free_address_space` умеет неполные деревья — общие с ядром узлы он пропускает).
pub(super) fn new_address_space() -> Option<usize> {
    let root = arch::clone_kernel_root()?;
    // Приватный стек в VPN[2]=1: несколько страниц из свежих фреймов.
    for i in 1..=USER_STACK_PAGES {
        let va = USER_STACK_TOP_VA - i * PAGE;
        let ok = frame::alloc().is_some_and(|pa| unsafe {
            arch::map(root, va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U)
        });
        if !ok {
            unsafe { arch::free_address_space(root) };
            return None;
        }
    }
    Some(root)
}

/// Веха 170, этап 3 — сбросить буфер трансляций после того, как в `space` ИЗМЕНИЛИСЬ уже
/// существовавшие отображения: у себя сразу, у соседей — рассылкой с ожиданием отчёта.
///
/// Нужно там, где отображение СНИМАЕТСЯ, ПЕРЕЗАПИСЫВАЕТСЯ или УРЕЗАЕТСЯ в правах. Для чистого
/// добавления (ленивая страница кучи) хватает своего сброса: процессор не заводит записей в
/// буфере для отсутствующих страниц, поэтому соседу нечего забывать, — а платить за каждый
/// фолт кучи межпроцессорным прерыванием пришлось бы на самом горячем пути ядра.
pub(super) fn flush_space(t: &Table, space: usize) {
    arch::flush_tlb();
    if cpu::MAX == 1 {
        return;
    }
    let me = cpu::id();
    let mut mask = 0usize;
    for (j, &tok) in t.space.iter().enumerate() {
        if j != me && tok == space {
            mask |= 1 << j;
        }
    }
    cpu::flush_others(mask);
}

/// Веха 170 — переехать на ядерный корень и объявить, что пространства процесса мы больше не
/// держим. Зовётся с большим замком: `Table::space` читают те, кто решает, что можно освободить.
pub(super) fn release_space(me: usize) {
    let mut t = TABLE.lock();
    if t.space[me] != 0 {
        // SAFETY: ядерный корень отображает и образ, и стеки, и прямую карту — всё, чем ядро
        // пользуется, пока не войдёт в процесс.
        unsafe { arch::mm_enable(arch::kernel_space_root()) };
        t.space[me] = 0;
    }
    t.bound[me] = NO_CPU;
}

/// Веха 22.2: page fault из U-mode. Фолт чтения/записи в ленивом диапазоне кучи
/// [`USER_HEAP_BASE_VA`, heap_brk) — выделить обнулённый фрейм, замапить `U|R|W` и повторить
/// инструкцию (sepc не двигаем). Любой другой фолт — включая исполнение кучи (W^X живёт и
/// здесь) и исчерпание фреймов — гибель ПРОЦЕССА, а не ядра: родителю в `SYS_EXEC` уходит MAX.
pub(super) fn handle_user_fault(t: &mut Table, cur: usize, va: usize, kind: FaultKind) {
    // Веха 35: куча — общая на группу нитей, её граница живёт у лидера (стек нити тоже
    // ленив и лежит в куче процесса, так что фолт стека любой нити резолвится отсюда).
    let (heap_brk, space) = (t.procs[t.procs[cur].group].heap_brk, t.procs[cur].space);
    let lazy = va >= USER_HEAP_BASE_VA && va < heap_brk && kind != FaultKind::Exec;
    let leader = t.procs[cur].group;
    if lazy && t.procs[leader].pages >= page_quota() {
        // Веха 89: аппетит исчерпал квоту — гибнет ИМЕННО этот процесс, соседи и ядро целы.
        println!(
            "  [mm] P{} превысил квоту памяти ({} страниц) — процесс убит (ядро живо)",
            cur,
            page_quota(),
        );
    } else if lazy {
        let page_va = va & !(PAGE - 1);
        // Веха 170 — страница УЖЕ отображена: её поставила соседняя нить той же группы, пока
        // мы стояли в очереди на большой замок с этим самым фолтом. Просто вернуться —
        // инструкция повторится и найдёт страницу на месте. Без этой проверки `map` положил бы
        // поверх второй фрейм: первый утёк бы, а всё, что нить успела в него записать, исчезло
        // бы без единого признака ошибки.
        if arch::translate(arch::space_root(space), page_va).is_some() {
            return;
        }
        if let Some(pa) = frame::alloc() {
            // SAFETY: пространство процесса сейчас активно — после map сбрасываем TLB,
            // иначе повтор инструкции мог бы увидеть старую (пустую) трансляцию.
            let ok = unsafe {
                arch::map(arch::space_root(space), page_va, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U)
            };
            if ok {
                t.procs[leader].pages += 1;
                // Своего сброса довольно: страница ДОБАВЛЕНА (см. `flush_space`).
                arch::flush_tlb();
                vprintln!("  [mm] P{} +страница {:#x} (ленивый фолт кучи)", cur, page_va);
                return; // sepc не тронут — инструкция повторится по замапленной странице
            }
            // Веха 89: памяти не хватило под ТАБЛИЦУ — фрейм назад и вниз, к общему пути
            // «процесс убит»: гибнет программа, которой не хватило памяти, а не ядро.
            frame::free(pa);
        }
        // Веха 199.13 — СМЕРТЬ ПРОЦЕССА СЛЫШНА ВСЕГДА, а не только при `log on`.
        //
        // Здесь стоял `vprintln!`, а в обычной сессии подробный вывод ВЫКЛЮЧЕН (`set_verbose(false)`
        // в `main`, Веха 82) — то есть процесс умирал молча. Это стоило целого расследования сети:
        // служба падала после первых же кадров, стека больше не было, и всё, что видел владелец, —
        // «сеть не отвечает», без единой строки о том, что отвечать давно некому.
        //
        // Падение — не трейс, а событие системы, и оно редкое по определению: журнал от него не
        // утонет, а вот тишина здесь стоит дней.
        println!(
            "  [mm] P{} '{}' фолт кучи {:#x}: памяти не хватило — ПРОЦЕСС УБИТ",
            cur, cap::domain_name(t.procs[cur].domain), va,
        );
    } else {
        println!(
            "  [mm] P{} '{}' page fault ({}) @ {:#x} pc {:#x} — ПРОЦЕСС УБИТ (ядро живо)",
            cur, cap::domain_name(t.procs[cur].domain), kind.name(), va, t.procs[cur].frame.pc(),
        );
    }
    t.procs[cur].state = State::Finished;
    lx_close_all(t, cur); // Веха 186: упавший тоже обязан отпустить трубы, иначе конвейер повиснет
    // Веха 195: упавший драйвер-карта перестаёт быть картой — иначе стек ждал бы кадров от
    // мертвеца, а «сети нет» выглядело бы как «сеть сломалась».
    crate::net::ext_detach(t.procs[cur].group);
    wake_exec_waiters(t, cur, usize::MAX);
    if let Some(n) = t.next_runnable(cur) {
        t.set_cur(n);
    }
}

/// Веха 223.6 — ЕСТЬ ЛИ У ПРОЦЕССА ТАКАЯ ПАМЯТЬ. Проверка перед тем, как ядро её тронет.
///
/// Не путать с [`ensure_heap_range`]: та отвечает за ЛЕНИВУЮ КУЧУ и всё, что кучей не является,
/// пропускает как «уже отображённое» — включая нулевой адрес. Для буфера, пришедшего из ЧУЖОЙ
/// программы, этого мало.
///
/// **Чем это стоило.** `read(0, NULL, 1)` из любой linux-программы доходил до цикла, который
/// пишет байты консоли прямо по переданному адресу, — и ядро получало page fault из S-mode,
/// то есть FATAL TRAP и смерть всей машины. Уронить VOID мог кто угодно одной строкой. Нашлось
/// при первом же запуске Rust-`std` (Веха 223.6), но дыра была с самого появления личности.
///
/// Нулевая страница отвергается отдельно и нарочно: она не принадлежит никому ни в одной
/// раскладке, а разыменование нуля — самая частая ошибка чужого кода.
pub(super) fn user_range_ok(t: &Table, pid: usize, va: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = va.checked_add(len - 1) else {
        return false; // переполнение — заведомо не чей-то буфер
    };
    if va < PAGE {
        return false;
    }
    let root = arch::space_root(t.procs[pid].space);
    let mut page = va & !(PAGE - 1);
    while page <= end {
        if arch::translate(root, page).is_none() {
            return false;
        }
        page += PAGE;
    }
    true
}

/// Веха 22.2: доотобразить ленивые страницы кучи ПЕРЕД тем, как ядро само тронет буфер
/// процесса в шлюзе (`OBJ_GET`/`OBJ_PUT`): фолт из S-mode мы не переживаем (fatal), поэтому
/// «ленивость» для ядра снимается заранее. Буферы вне кучи (стек, данные ELF) замаплены и так.
/// `false` — диапазон в куче, но фреймы кончились (шлюзу следует отказать).
pub(super) fn ensure_heap_range(t: &Table, pid: usize, va: usize, len: usize) -> bool {
    // Веха 35: граница кучи — у лидера группы (нити делят кучу процесса).
    if len == 0 || va < USER_HEAP_BASE_VA || va.saturating_add(len) > t.procs[t.procs[pid].group].heap_brk {
        return true; // не куча — обычные (уже отображённые) страницы
    }
    let root = arch::space_root(t.procs[pid].space);
    let mut page = va & !(PAGE - 1);
    while page < va + len {
        if arch::translate(root, page).is_none() {
            if t.procs[t.procs[pid].group].pages >= page_quota() {
                return false; // Веха 89: квота исчерпана — шлюз откажет, процесс жив
            }
            let Some(pa) = frame::alloc() else { return false };
            if !unsafe { arch::map(root, page, pa, arch::MAP_R | arch::MAP_W | arch::MAP_U) } {
                frame::free(pa);
                return false; // Веха 89: нет памяти под таблицу — шлюз честно откажет
            }
            // Своего сброса довольно: страница ДОБАВЛЕНА (см. `flush_space`).
            arch::flush_tlb();
            vprintln!("  [mm] P{} +страница {:#x} (доотображение под шлюз)", pid, page);
        }
        page += PAGE;
    }
    true
}

/// Веха 89 — **КВОТА СТРАНИЦ на группу нитей**. Раньше её не было вовсе: программа, которая
/// в цикле трогает новые страницы кучи, забирала всю RAM машины, и следующим падал не автор
/// аппетита, а тот, кому не досталось, — вплоть до ядра.
///
/// Квота относительная (четверть рабочей RAM, но не меньше 8 МиБ): на 128-МиБ QEMU это 32 МиБ,
/// на реальной машине с гигабайтами — гигабайты. Абсолютная константа тут врала бы в обе
/// стороны. Считается один раз: карта памяти после загрузки не меняется.
pub(super) fn page_quota() -> usize {
    const MIN: usize = 8 * 1024 * 1024;
    (crate::frame::usable_bytes() / 4).max(MIN) / PAGE
}

/// Веха 46 — собрать корни адресных пространств тех групп, где ВСЕ нити уже `Finished`, и
/// пометить их `space = RECLAIMED` (чтобы не освободить дважды и не тронуть устаревший корень).
/// Единая точка для всех путей гибели (SYS_EXIT, linux exit_group, page fault, лишняя нить):
/// группа освобождается ровно тогда, когда в ней не осталось живых нитей. Возвращает корни —
/// САМО освобождение делает [`resume`] уже под живым пространством (рушить таблицы под
/// активным satp/CR3 нельзя).
pub(super) fn reclaim_dead_spaces(t: &mut Table) -> Vec<usize> {
    let mut roots = Vec::new();
    let n = t.procs.len();
    // Веха 170.7 — сперва спросить, есть ли вообще что возвращать.
    //
    // Ниже — двойной обход: на каждого лидера проверяется вся таблица. Зовётся это на КАЖДОМ
    // трапе из процесса, а умирает кто-нибудь раз в сотни тысяч трапов, — то есть почти всегда
    // квадрат отрабатывал впустую. Пока программ было пять, это терялось в шуме; с двумя
    // десятками открытых окон таблица разрастается, и цена растёт как квадрат числа слотов.
    // Одна линейная проверка отсекает почти все заходы.
    if !t.procs.iter().any(|p| p.state == State::Finished && p.space != RECLAIMED) {
        return roots;
    }
    for leader in 0..n {
        if t.procs[leader].group != leader || t.procs[leader].space == RECLAIMED {
            continue; // только лидеры групп и только ещё не освобождённые
        }
        let all_dead =
            (0..n).all(|i| t.procs[i].group != leader || t.procs[i].state == State::Finished);
        if !all_dead {
            continue;
        }
        roots.push(arch::space_root(t.procs[leader].space));
        // Веха 156 — группа умерла целиком: отпустить её домен. Нити делят c-space лидера, поэтому
        // отпускается он один раз, а не по разу на нить. Канонический домен ждёт следующего тёзку
        // с правами прошлой жизни, временный — исчезает вместе с процессом.
        cap::release_domain(t.procs[leader].domain);
        for i in 0..n {
            if t.procs[i].group == leader {
                t.procs[i].space = RECLAIMED;
                // Веха 89 — сперва ОТОЗВАТЬ права, указывающие на этот номер (эндпоинты и
                // reply), и только потом отдать слот под переиспользование: иначе устаревший
                // cap начал бы адресовать чужой, новый процесс.
                cap::revoke_process(i);
                // Веха 129 — отпустить области разделяемой памяти. Страницы в них общие:
                // освободит их та, у которой уйдёт последний держатель, а не этот процесс.
                for id in core::mem::take(&mut t.procs[i].shm) {
                    crate::shm::release(id);
                }
                // Веха 97 — умер владелец ЭКРАНА: вернуть экран ядру. Без этого терминал,
                // упавший или вышедший, оставлял бы систему немой — ядро продолжало бы считать
                // экран занятым и печатать в один serial.
                if arch::video_owner() == Some(i) {
                    arch::video_take_back();
                    println!("  [видео] владелец экрана P{} завершился — экран вернулся ядру", i);
                }
                // Веха 98 — слот ЗОМБИ придержан: родитель ещё не забрал код выхода
                // (`SYS_WAIT`). Отдать его сейчас значит подсунуть ожидающему чужой процесс.
                // Текущий слот не отдаём по другой причине: `resume` ещё читает из него кадр.
                if i != t.cur() && !t.procs[i].zombie {
                    t.free_slots.push(i);
                }
            }
        }
    }
    roots
}

/// Веха 129 — создать область разделяемой памяти и отобразить её себе. Возвращает биты права
/// на область или `usize::MAX`.
pub(super) fn shm_map_new(t: &mut Table, cur: usize, len: usize, va: usize) -> usize {
    let Some(id) = crate::shm::create(len) else { return usize::MAX };
    if shm_attach(t, cur, id, va, true) == usize::MAX {
        crate::shm::release(id); // отобразить не вышло — область никому не нужна
        return usize::MAX;
    }
    let dom = t.procs[cur].domain;
    // GRANT — не щедрость, а смысл области: она заводится ради того, чтобы ПОДЕЛИТЬСЯ, а отправка
    // права по IPC требует GRANT (проверяется на `CALL`). Без него создатель владел бы буфером,
    // которым не может ни с кем поделиться, — то есть обычной памятью.
    //
    // Урезает права уже сам создатель (`CAP_DERIVE`) перед отправкой: композитору уезжает
    // READ|GRANT без WRITE, и в кадр он писать не может.
    cap::mint(dom, cap::Target::Shm(id), Rights::READ.union(Rights::WRITE).union(Rights::GRANT))
        .bits() as usize
}

/// Отобразить УЖЕ существующую область (право проверено вызывающим).
pub(super) fn shm_map_existing(t: &mut Table, cur: usize, id: usize, va: usize, writable: bool) -> usize {
    if !crate::shm::retain(id) {
        return usize::MAX;
    }
    if shm_attach(t, cur, id, va, writable) == usize::MAX {
        crate::shm::release(id);
        return usize::MAX;
    }
    crate::shm::len(id)
}

/// Общая часть: разложить страницы области по адресам процесса начиная с `va`.
///
/// Диапазон проверяется теми же границами, что и у кучи с DMA: чужое отображение обязано лечь в
/// пользовательскую область, а не поверх ядра или стека.
pub(super) fn shm_attach(t: &mut Table, cur: usize, id: usize, va: usize, writable: bool) -> usize {
    let frames = crate::shm::frames(id);
    if frames.is_empty() || va % PAGE != 0 {
        return usize::MAX;
    }
    let limit = USER_STACK_TOP_VA - USER_STACK_PAGES * PAGE;
    if va < USER_REGION_START || va + frames.len() * PAGE > limit {
        return usize::MAX;
    }
    let root = arch::space_root(t.procs[cur].space);
    // MAP_SHARED — пометка в записи: смерть процесса не должна освобождать общие страницы
    // (`free_private` такие листья пропускает).
    let mut flags = arch::MAP_R | arch::MAP_U | arch::MAP_SHARED;
    if writable {
        flags |= arch::MAP_W;
    }
    let space = t.procs[cur].space;
    for (i, &pa) in frames.iter().enumerate() {
        let ok = unsafe { arch::map(root, va + i * PAGE, pa, flags) };
        if !ok {
            flush_space(t, space);
            return usize::MAX; // Веха 89: нет памяти под таблицу — честный отказ
        }
    }
    // Веха 170 — адрес выбирает процесс, значит `map` мог ПЕРЕЗАПИСАТЬ то, что там лежало:
    // сброс нужен всем, кто стоит на этом пространстве.
    flush_space(t, space);
    t.procs[cur].shm.push(id);
    0
}

/// Веха 129 — отпустить область: снять её страницы с адресов процесса и убавить держателя.
///
/// Без этого разделяемая память была бы механизмом без второй половины: буфер кадра меняется на
/// КАЖДУЮ смену размера окна (в тайлинге — при появлении соседа), и область, которую никто не
/// отпускает, оставалась бы висеть до смерти процесса. Пара окон, пожившая под перекладыванием,
/// съедала бы память гарантированно.
///
/// Проверки — «страницы на месте и это ТЕ САМЫЕ страницы»; всё, чего процесс добьётся ложью в
/// аргументах, — отказ. Отпускаем ВСЁ ИЛИ НИЧЕГО: полуснятое отображение это буфер, часть
/// которого уже чужая, — а такое обнаружится далеко от места ошибки.
pub(super) fn shm_unmap(t: &mut Table, cur: usize, id: usize, va: usize) -> usize {
    // Держит ли он её вообще: право даёт доступ, а отпустить можно лишь СВОЁ отображение.
    let Some(slot) = t.procs[cur].shm.iter().position(|&x| x == id) else { return usize::MAX };
    let frames = crate::shm::frames(id);
    if frames.is_empty() || va % PAGE != 0 {
        return usize::MAX;
    }
    let root = arch::space_root(t.procs[cur].space);
    // Сперва проверяем весь диапазон, потом снимаем: иначе ошибка на середине оставила бы
    // процесс с половиной буфера.
    for (i, &pa) in frames.iter().enumerate() {
        if arch::translate(root, va + i * PAGE) != Some(pa) {
            return usize::MAX;
        }
    }
    for (i, &pa) in frames.iter().enumerate() {
        let ok = unsafe { arch::unmap_shared(root, va + i * PAGE, pa) };
        // Отказ здесь — не ложь процесса (её отсеял проход выше), а нарушение инварианта ЯДРА:
        // по адресу лежит фрейм области, но помечен он не общим, то есть один фрейм роздан
        // дважды. Такое чинить на месте нечем и молчать о таком нельзя — память уже портится.
        assert!(ok, "shm_unmap: фрейм области отображён как приватный (va {:#x}, P{})",
            va + i * PAGE, cur);
    }
    // Веха 170 — страницы СНЯТЫ, и сразу за этим область может быть отпущена вместе с кадрами.
    // Сосед со старой трансляцией писал бы в чужую память: рассылаем и ждём отчёта.
    flush_space(t, t.procs[cur].space);
    t.procs[cur].shm.remove(slot);
    crate::shm::release(id);
    0
}

/// Скопировать `src` в адресное пространство с корнем `root` по виртуальному адресу `dst_va`,
/// постранично транслируя (страницы процесса не отображены идентично). Физический адрес назначения
/// доступен ядру через идентичное отображение RAM, поэтому переключать `satp` не нужно.
pub(super) fn copy_to_space(root: usize, mut dst_va: usize, src: &[u8]) {
    let mut off = 0;
    while off < src.len() {
        let Some(pa) = arch::translate(root, dst_va) else { return };
        let page_off = dst_va & (PAGE - 1);
        let n = (src.len() - off).min(PAGE - page_off);
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(off), crate::frame::ptr(pa), n) };
        off += n;
        dst_va += n;
    }
}

/// Скопировать `len` байт МЕЖДУ двумя адресными пространствами: из `src_va` (корень `src_root`)
/// в `dst_va` (корень `dst_root`). Оба конца транслируем постранично в физические адреса (RAM
/// идентично отображена в ядре → переключать `satp` не нужно); шаг ограничен границей страницы
/// с обеих сторон, т.к. буферы могут пересекать страницы независимо.
pub(super) fn copy_between_spaces(
    src_root: usize,
    mut src_va: usize,
    dst_root: usize,
    mut dst_va: usize,
    len: usize,
) {
    let mut off = 0;
    while off < len {
        let (Some(spa), Some(dpa)) =
            (arch::translate(src_root, src_va), arch::translate(dst_root, dst_va))
        else {
            return;
        };
        let s_off = src_va & (PAGE - 1);
        let d_off = dst_va & (PAGE - 1);
        let n = (len - off).min(PAGE - s_off).min(PAGE - d_off);
        // Оба конца — физические адреса из translate; ходим по ним через direct-map.
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::frame::ptr(spa) as *const u8,
                crate::frame::ptr(dpa),
                n,
            )
        };
        off += n;
        src_va += n;
        dst_va += n;
    }
}
