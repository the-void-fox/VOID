//! Веха 214.6 — системные вызовы: **объекты, корни и диск**.
//!
//! Содержимое по хэшу, именованные корни, узлы дерева, сборка мусора и прямой доступ к блочному
//! устройству. Здесь VOID отвечает на «что лежит в системе».
//!
//! Разбор номера — в [`super`]; сюда он приходит уже разобранным. Деление введено затем, что
//! диспетчер был одной функцией на две с половиной тысячи строк: в такую нельзя заглянуть
//! целиком, а значит нельзя и убедиться, что рукава не мешают друг другу.

use super::super::*;

/// Обработать вызов, если он наш. `false` — не наш, пусть смотрит следующий.
pub(super) fn dispatch(t: &mut Table, cur: usize, num: usize) -> bool {
    match num {
        // SYS_BLK_READ(dev_cap, sector, buf): шлюз к диску ПОД ЗАЩИТОЙ capability. Без валидного
        // cap на устройство (право READ) — отказ, даже если процесс знает номер сектора. DMA идёт
        // в ЯДЕРНЫЙ буфер (страницы процесса не identity-mapped), затем копируем вызывающему (SUM=1).
        7 => {
            let (dcap, sector, ubuf) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::READ) {
                // Веха 23: приёмный буфер может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, 512) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ сектор {} (по cap)", cur, sector);
                    let mut tmp = [0u8; 512];
                    let ok = crate::virtio_blk::read(sector as u64, &mut tmp);
                    if ok {
                        let dst = unsafe { core::slice::from_raw_parts_mut(ubuf as *mut u8, 512) };
                        dst.copy_from_slice(&tmp);
                    }
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_READ отклонён: {:?}  ← нет capability на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT(store_cap, buf, len, id_out) -> 0/MAX: сохранить значение в объектный
        // [[object-model|store]] (нужен cap на store с правом WRITE) и записать 32-байтный
        // content-id в id_out. Буферы читаются/пишутся в пространстве вызывающего (он current, SUM=1).
        8 => {
            let (scap, buf, len, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 22.2: буфер (и id_out — Веха 23) может лежать в ленивой куче —
                // доотобразить до того, как ядро его тронет.
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    // Веха 104 — нехватка памяти ядра здесь ОТКАЗ, а не паника: размер задаёт
                    // программа (а в пакетной фазе — сеть и чужой архив), и падать всей системой
                    // на чужой цифре недопустимо.
                    match crate::object::try_put(bytes) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!("  [obj] P{} OBJ_PUT {} байт → content-id (по cap)", cur, len);
                            0
                        }
                        None => {
                            println!("  [obj] P{} OBJ_PUT {} байт: НЕ ХВАТИЛО памяти ядра", cur, len);
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET(store_cap, id_ptr, out_buf, out_cap) -> длина (0 — нет; MAX — отказ):
        // прочитать значение по 32-байтному content-id (нужен cap на store с правом READ).
        //
        // Веха 114 — ВТОРЫМ значением возвращается НАСТОЯЩАЯ длина объекта. Без неё «объект ровно
        // с буфер» и «объект не влез» выглядели одинаково, и читатели росли удвоением буфера,
        // перечитывая объект по нескольку раз. Второе значение прежних читателей не задевает
        // (они берут только первое) — та же уловка, которой Веха 101 добавила «сколько хотели
        // отдать» к IPC.
        9 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let (result, whole) = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 22.2: приёмный буфер (и id_ptr — Веха 23) может лежать в ленивой куче —
                // доотобразить до записи ядром (весь ocap: лениво он выделился бы всё равно).
                Ok(()) if ensure_heap_range(t, cur, obuf, ocap)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    let (n, whole) = crate::object::with(&ContentId(id), |b| match b {
                        Some(bytes) => {
                            let m = bytes.len().min(ocap);
                            let out = unsafe { core::slice::from_raw_parts_mut(obuf as *mut u8, m) };
                            out.copy_from_slice(&bytes[..m]);
                            (m, bytes.len())
                        }
                        None => (0, 0),
                    });
                    vprintln!("  [obj] P{} OBJ_GET → {} байт из {} (по cap)", cur, n, whole);
                    (n, whole)
                }
                Ok(()) => (usize::MAX, 0), // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET отклонён: {:?}  ← нет capability на store", cur, e);
                    (usize::MAX, 0)
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.set_ret_at(1, whole);
            f.advance();
        }
        // SYS_OBJ_SET_ROOT(store_cap, name_ptr, name_len, id_ptr) -> 0/MAX: привязать именованный
        // корень к значению (нужен `WRITE`). Так объект переживает перезагрузку ([[persistent-store]]).
        10 => {
            let (scap, nptr, nlen, idp) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя и id могут лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idp, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    let mut id = [0u8; 32];
                    let src = unsafe { core::slice::from_raw_parts(idp as *const u8, 32) };
                    id.copy_from_slice(src);
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            crate::object::set_root(name, ContentId(id));
                            // Веха 33: чекпойнт-на-каждый-чих сменился group commit —
                            // операция лишь копит счётчик, фиксацию делает политика
                            // ([`object::maybe_commit`] в resume(): порог или ~2 с).
                            vprintln!("  [obj] P{} OBJ_SET_ROOT '{}' (по cap, в пачку)", cur, name);
                            0
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_SET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GET_ROOT(store_cap, name_ptr, name_len, id_out) -> 32 (есть) / 0 (нет) / MAX
        // (отказ): узнать content-id именованного корня (нужен `READ`).
        11 => {
            let (scap, nptr, nlen, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                // Веха 23: имя и id_out могут лежать в ленивой куче — доотобразить.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen)
                    && ensure_heap_range(t, cur, idout, 32) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => match crate::object::root(name) {
                            Some(id) => {
                                let out = unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                                out.copy_from_slice(&id.0);
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → есть (по cap)", cur, name);
                                32
                            }
                            None => {
                                vprintln!("  [obj] P{} OBJ_GET_ROOT '{}' → нет (по cap)", cur, name);
                                0
                            }
                        },
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_GET_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_BLK_WRITE(dev_cap, sector, buf, len) -> 0/MAX: записать сектор ПОД ЗАЩИТОЙ capability
        // (нужен `WRITE` на устройство). Данные копируем из буфера вызывающего (SUM=1) в ЯДЕРНЫЙ
        // буфер (страницы процесса не identity-mapped для DMA), недостающее до сектора — нулями.
        12 => {
            let (dcap, sector, ubuf, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::device(dom, Cap::from_bits(dcap as u64), Rights::WRITE) {
                // Веха 23: буфер данных может лежать в ленивой куче — доотобразить.
                Ok(cap::Device::Block) if ensure_heap_range(t, cur, ubuf, len.min(512)) => {
                    let mut tmp = [0u8; 512];
                    let n = len.min(512);
                    let src = unsafe { core::slice::from_raw_parts(ubuf as *const u8, n) };
                    tmp[..n].copy_from_slice(src);
                    let ok = crate::virtio_blk::write(sector as u64, &tmp);
                    vprintln!("  [blk] P{} SYS_BLK_WRITE сектор {} ({} байт, по cap)", cur, sector, n);
                    if ok { 0 } else { usize::MAX }
                }
                Ok(_) => usize::MAX, // право есть, а фреймов под ленивый буфер нет
                Err(e) => {
                    vprintln!("  [blk] P{} SYS_BLK_WRITE отклонён: {:?}  ← нет capability (WRITE) на устройство", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_DEL_ROOT(store_cap, name_ptr, name_len) -> 0 (снят) / 1 (не было) / MAX (отказ):
        // отвязать именованный корень (нужен `WRITE`). Объект уходит в GC, если больше ни на что не
        // сослан — это делает `unlink` в персоналии честным (Веха 18.3).
        13 => {
            let (scap, nptr, nlen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                // Веха 23: имя может лежать в ленивой куче — доотобразить до чтения ядром.
                Ok(()) if ensure_heap_range(t, cur, nptr, nlen) => {
                    let name_bytes = unsafe { core::slice::from_raw_parts(nptr as *const u8, nlen) };
                    match core::str::from_utf8(name_bytes) {
                        Ok(name) => {
                            // Веха 33: снятие корня тоже едет пачкой (group commit).
                            let existed = crate::object::del_root(name);
                            vprintln!("  [obj] P{} OBJ_DEL_ROOT '{}' → {} (по cap)", cur, name, if existed { "снят" } else { "не было" });
                            if existed { 0 } else { 1 }
                        }
                        Err(_) => usize::MAX,
                    }
                }
                Ok(()) => usize::MAX, // куча есть, а фреймов нет
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_DEL_ROOT отклонён: {:?}  ← нет capability на store", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_INSTALL(store_cap, op, slot, buf, len) (Вехи 48, 174): установить VOID на SATA-диск
        // из загрузочного модуля multiboot2 (образ с носителя) — либо СПРОСИТЬ, какие диски есть.
        //
        //   op = 0 — перечислить диски: в `buf` пишутся записи по [`INSTALL_REC`] байт, возврат —
        //            сколько записано. Столько же, сколько влезло в `len`.
        //   op = 1 — установить на диск с номером `slot`; возврат — сектор начала store | MAX.
        //
        // Оба под одним правом — store-cap с WRITE (у shell'а `store:xw`): установка меняет
        // содержимое store целиком, право по силе равно записи. Список дисков сам по себе
        // безобиден, но отдельного права под него мы не заводим: спрашивает его ровно тот, кто
        // собирается ставить, а лишний вид права — лишняя вещь, которую надо объяснять.
        //
        // ДИСК СТИРАЕТСЯ ЦЕЛИКОМ. Ставить на диск, с которого работает система, ядро отказывает.
        30 => {
            let (scap, op, slot, ptr, len) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Err(e) => {
                    vprintln!("  [install] P{} отклонён: {:?}  ← нет capability (WRITE) на store", cur, e);
                    usize::MAX
                }
                Ok(()) if op == 0 => {
                    let want = (len / INSTALL_REC).min(crate::arch::MAX_DISKS);
                    let mut disks = [crate::ahci::Disk {
                        slot: 0,
                        sectors: 0,
                        model: [0; crate::ahci::MODEL_LEN],
                        void: false,
                        live: false,
                    }; crate::arch::MAX_DISKS];
                    let n = crate::ahci::disks(&mut disks[..want]);
                    // Веха 194.1: NVMe-диски идут ПОСЛЕ портов AHCI — своими номерами (сотня),
                    // чтобы выбор человека значил одно и то же и на машине с двумя шинами.
                    let n = n + crate::nvme::disks(&mut disks[n..want]);
                    // Веха 196 — и USB-накопители, своей сотней (200+). Порядок тот же:
                    // сперва внутренние шины, потом съёмное.
                    let n = n + crate::xhci::disks(&mut disks[n..want]);
                    if !ensure_heap_range(t, cur, ptr, n * INSTALL_REC) {
                        usize::MAX
                    } else {
                        for (i, d) in disks.iter().take(n).enumerate() {
                            // SAFETY: диапазон проверен `ensure_heap_range` — он же дотянул
                            // ленивые страницы кучи, в которые пишем.
                            let rec = unsafe {
                                core::slice::from_raw_parts_mut(
                                    (ptr + i * INSTALL_REC) as *mut u8,
                                    INSTALL_REC,
                                )
                            };
                            rec.fill(0);
                            rec[0..8].copy_from_slice(&d.sectors.to_le_bytes());
                            rec[8..12].copy_from_slice(&(d.slot as u32).to_le_bytes());
                            rec[12] = (d.void as u8) | (d.live as u8) << 1;
                            rec[16..16 + crate::ahci::MODEL_LEN].copy_from_slice(&d.model);
                        }
                        n
                    }
                }
                // Веха 222 — op 2: ОБНОВИТЬ, не трогая данные (разбор в `install::update`).
                Ok(()) if op == 2 => match crate::install::update(slot) {
                    Ok(p2) => {
                        crate::println!(
                            "  [install] VOID обновлён на диске {} (store с сектора {} не тронут) — перезагрузись без носителя",
                            slot, p2
                        );
                        p2 as usize
                    }
                    Err(e) => {
                        crate::println!("  [install] обновление отклонено: {}", e);
                        usize::MAX
                    }
                },
                Ok(()) => match crate::install::run(slot) {
                    Ok(p2) => {
                        crate::println!("  [install] VOID установлен на диск {} (store с сектора {}) — перезагрузись без носителя", slot, p2);
                        p2 as usize
                    }
                    Err(e) => {
                        crate::println!("  [install] отказ: {}", e);
                        usize::MAX
                    }
                },
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_LIST_ROOTS(store_cap, buf_ptr, buf_len) -> ПОЛНАЯ длина текста | MAX:
        // перечислить СЫРЫЕ корни store текстом («короткий id + имя» на строку) — vsh `roots`,
        // как `ls` для объектов store. Гейт: store-cap с READ ИЛИ WRITE (любой из
        // привилегированных доступов к store позволяет узнать имена корней; у shell'а cap
        // store:xw — есть WRITE).
        //
        // Веха 107: возвращается длина ВСЕГО текста, а не записанного. Раньше отдавалось
        // `min(длина, буфер)` — и «корней ровно столько» было не отличить от «буфер мал», причём
        // обрезание приходилось на середину строки: имя корня доезжало покалеченным. На этом
        // стоит нумерация поколений (`system/gen<N>`, `pkg/profile/*/gen<N>`), а `pkg` заводит
        // по два корня на каждый путь замыкания — недосчитаться поколения значило бы ЗАТЕРЕТЬ
        // существующее. Соглашение то же, что у SYS_OBJ_CHILDREN и readdir персоналии.
        34 => {
            let (scap, bptr, blen) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2))
            };
            let dom = t.procs[cur].domain;
            let cap = Cap::from_bits(scap as u64);
            let allowed = cap::store(dom, cap, Rights::READ).is_ok()
                || cap::store(dom, cap, Rights::WRITE).is_ok();
            let result = if !allowed {
                vprintln!("  [obj] P{} OBJ_LIST_ROOTS отклонён ← нет capability (READ/WRITE) на store", cur);
                usize::MAX
            } else if ensure_heap_range(t, cur, bptr, blen) {
                let text = crate::object::list_roots_text();
                let bytes = text.as_bytes();
                let n = bytes.len().min(blen);
                let dst = unsafe { core::slice::from_raw_parts_mut(bptr as *mut u8, n) };
                dst.copy_from_slice(&bytes[..n]);
                vprintln!(
                    "  [obj] P{} OBJ_LIST_ROOTS → {} Б из {} ({} корней)",
                    cur, n, bytes.len(), text.lines().count()
                );
                bytes.len()
            } else {
                usize::MAX // куча под буфер не доотобразилась
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_GC(store_cap) -> собрано объектов | MAX: сборка мусора store по достижимости
        // от корней (Веха 109). Нужен store-cap с WRITE: это операция, меняющая store.
        //
        // Наружу она понадобилась пакетам: `pkg gc` снимает корни путей, выпавших из всех
        // поколений профиля, — но пока никто не пройдёт по графу, место занято по-прежнему.
        // Раньше сборка случалась только на загрузке, то есть «удалил — перезагрузись».
        46 => {
            let scap = t.procs[cur].frame.arg(0);
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) => {
                    let (kept, collected) = crate::object::gc();
                    println!(
                        "  [gc] P{} по запросу: достижимо {}, собрано {}",
                        cur, kept, collected
                    );
                    collected
                }
                Err(e) => {
                    vprintln!("  [obj] P{} SYS_OBJ_GC отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_PUT_NODE(store_cap, buf, len, kids_ptr, nkids, idout) -> 0|MAX (Веха 94):
        // положить УЗЕЛ — значение плюс список исходящих ссылок (по 32 байта каждая).
        //
        // Зачем отдельно от `SYS_OBJ_PUT`: большой файл не кладётся одним слайсом — ни в кучу
        // процесса, ни в кучу ядра. Он кладётся КУСКАМИ (каждый — обычный объект), а узел
        // связывает их в целое. Дедуп при этом достаётся даром: одинаковый кусок в двух
        // загрузках — один объект. GC уже умеет ходить по детям (checkpoint строит такое же
        // дерево с Вехи 37), так что новой машинерии не появляется — только доступ из userspace.
        38 => {
            let (scap, buf, len, kids, nkids, idout) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3), f.arg(4), f.arg(5))
            };
            let dom = t.procs[cur].domain;
            let kbytes = nkids.saturating_mul(32);
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::WRITE) {
                Ok(()) if ensure_heap_range(t, cur, buf, len)
                    && (nkids == 0 || ensure_heap_range(t, cur, kids, kbytes))
                    && ensure_heap_range(t, cur, idout, 32) =>
                {
                    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                    let mut children = Vec::with_capacity(nkids);
                    for i in 0..nkids {
                        let mut id = [0u8; 32];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (kids + i * 32) as *const u8, id.as_mut_ptr(), 32,
                            )
                        };
                        children.push(void_abi::ContentId(id));
                    }
                    // Веха 104 — нехватка памяти ядра: отказ, а не паника (см. OBJ_PUT).
                    match crate::object::try_put_node(bytes, &children) {
                        Some(id) => {
                            let out =
                                unsafe { core::slice::from_raw_parts_mut(idout as *mut u8, 32) };
                            out.copy_from_slice(&id.0);
                            vprintln!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт + {} детей → content-id (по cap)",
                                cur, len, nkids,
                            );
                            0
                        }
                        None => {
                            println!(
                                "  [obj] P{} OBJ_PUT_NODE {} байт: НЕ ХВАТИЛО памяти ядра",
                                cur, len,
                            );
                            usize::MAX
                        }
                    }
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_PUT_NODE отклонён: {:?}", cur, e);
                    usize::MAX
                }
            };
            let f = &mut t.procs[cur].frame;
            f.set_ret(result);
            f.advance();
        }
        // SYS_OBJ_CHILDREN(store_cap, id_ptr, out_buf, out_cap) -> число детей | MAX (Веха 94):
        // выписать ссылки узла (по 32 байта). Без этого положенное деревом нельзя прочитать
        // обратно: `SYS_OBJ_GET` отдаёт только полезную нагрузку узла, а не его детей.
        39 => {
            let (scap, idp, obuf, ocap) = {
                let f = &t.procs[cur].frame;
                (f.arg(0), f.arg(1), f.arg(2), f.arg(3))
            };
            let dom = t.procs[cur].domain;
            let result = match cap::store(dom, Cap::from_bits(scap as u64), Rights::READ) {
                Ok(()) if ensure_heap_range(t, cur, idp, 32)
                    && (ocap == 0 || ensure_heap_range(t, cur, obuf, ocap)) =>
                {
                    let mut id = [0u8; 32];
                    unsafe { core::ptr::copy_nonoverlapping(idp as *const u8, id.as_mut_ptr(), 32) };
                    let kids = crate::object::children(&void_abi::ContentId(id));
                    let n = kids.len().min(ocap / 32);
                    for (i, c) in kids.iter().take(n).enumerate() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                c.0.as_ptr(), (obuf + i * 32) as *mut u8, 32,
                            )
                        };
                    }
                    // Возвращаем ПОЛНОЕ число детей, а не сколько влезло: иначе вызывающий не
                    // отличил бы «детей ровно столько» от «буфер мал» и потерял бы хвост.
                    kids.len()
                }
                Ok(()) => usize::MAX,
                Err(e) => {
                    vprintln!("  [obj] P{} OBJ_CHILDREN отклонён: {:?}", cur, e);
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
