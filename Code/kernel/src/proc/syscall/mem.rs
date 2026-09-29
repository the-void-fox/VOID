//! Веха 214.6 — системные вызовы: **память процесса**.
//!
//! Ленивая куча, окна регистров устройств, память под DMA и общие области. Работу делает
//! [`super::super::space`]; здесь — разбор аргументов и проверка прав.
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
    match num {
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
        _ => return false,
    }
    true
}
