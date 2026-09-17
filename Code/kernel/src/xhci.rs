//! Драйвер USB xHCI — часть A (Веха 50): подъём контроллера (HCD) и машинерия колец.
//!
//! Самый большой драйвер: USB-стек идёт по частям — (A) контроллер: сброс, DCBAA, кольцо
//! команд, кольцо событий, запуск, детект устройства на порту + Enable Slot (доказывает, что
//! кольца/дверные звонки/события работают); (B) перечисление устройства (control-трансферы,
//! Address Device, дескрипторы); (C) HID-клавиатура (interrupt-эндпоинт → скан-коды в консоль).
//!
//! Опрос, без прерываний (как остальные наши драйверы). Все кольца/массивы — в обнулённых
//! фреймах [`crate::frame`] (RAM отображена идентично, контроллер читает их DMA; на x86 DMA
//! когерентен). На riscv/QEMU-virt xHCI нет — `probe` вернёт `None`, [`init`] тихо откажется.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};

use crate::frame;
use crate::sync::SpinLock;

// ─── регистры capability (от базы BAR0) ──────────────────────────────────────
const CAP_CAPLENGTH: usize = 0x00; // u8: длина cap-регистров = смещение операционных
const CAP_HCSPARAMS1: usize = 0x04; // u32: MaxSlots[7:0], MaxPorts[31:24]
const CAP_HCCPARAMS1: usize = 0x10; // u32: CSZ (контекст 64 Б) бит2
const CAP_DBOFF: usize = 0x14; // u32: смещение массива дверных звонков
const CAP_RTSOFF: usize = 0x18; // u32: смещение runtime-регистров

// ─── операционные регистры (от op_base = BAR0 + CAPLENGTH) ───────────────────
const OP_USBCMD: usize = 0x00; // RS бит0, HCRST бит1
const OP_USBSTS: usize = 0x04; // HCH бит0, CNR бит11
const OP_CRCR: usize = 0x18; // u64: кольцо команд + RCS бит0
const OP_DCBAAP: usize = 0x30; // u64: массив базовых адресов контекстов устройств
const OP_CONFIG: usize = 0x38; // MaxSlotsEn[7:0]
const OP_PORTS: usize = 0x400; // PORTSC[port] = OP_PORTS + (port-1)*0x10

const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

const PORTSC_CCS: u32 = 1 << 0; // устройство подключено
const PORTSC_PED: u32 = 1 << 1; // порт включён
const PORTSC_PR: u32 = 1 << 4; // сброс порта
// Биты-изменения (RW1C) в PORTSC — пишем 1, чтобы сбросить, не трогая PED/CCS.
const PORTSC_CHANGES: u32 = 0x00fe_0000;

// ─── runtime: интерраптер 0 (от rt_base + 0x20) ──────────────────────────────
const IR0_ERSTSZ: usize = 0x08; // размер таблицы сегментов
const IR0_ERSTBA: usize = 0x10; // u64: база таблицы сегментов
const IR0_ERDP: usize = 0x18; // u64: указатель извлечения событий
const ERDP_EHB: u64 = 1 << 3; // Event Handler Busy (пишем 1 при обновлении)

const RING_TRBS: usize = 256; // TRB в кольце (одно 4 КиБ-фрейм / 16)
/// Сектор накопителя. Тот же, что у AHCI/NVMe: выше слоя носителя разницы быть не должно.
const SECTOR: usize = 512;

/// Сколько буферов приёма держим у клавиатуры НЕЗАКРЫТЫМИ (Веха 196).
///
/// Был один, и половина нажатий терялась: пока мы разбираем репорт и подставляем буфер обратно,
/// устройству писать некуда. При наборе строки это выглядело как «клавиатура печатает через
/// букву» — хуже, чем если бы она не работала вовсе, потому что похоже на случайность.
/// Восемь — с запасом на четыре быстрых нажатия (репорт на нажатие и на отпускание).
const KBD_REPORTS: usize = 8;
const TRB_NORMAL: u32 = 1; // Normal (данные interrupt/bulk)
const TRB_SETUP: u32 = 2; // Setup Stage (control-трансфер)
const TRB_DATA: u32 = 3; // Data Stage
const TRB_STATUS: u32 = 4; // Status Stage
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDR_DEV: u32 = 11; // Address Device
const TRB_CONFIG_EP: u32 = 12; // Configure Endpoint
const EV_TRANSFER: u32 = 32; // Transfer Event
const EV_CMD_COMPLETE: u32 = 33; // Command Completion Event

#[allow(dead_code)] // op/db/rt/max_ports/ctx64/dcbaa частью пригодятся в B/C (перечисление, HID)
/// Сколько устройств перечисляем. Веха 196 — их стало НЕСКОЛЬКО: раньше драйвер брал первый
/// порт с устройством и на этом останавливался, поэтому клавиатура и флешка были
/// взаимоисключающими — какая раньше в списке портов, та и работает. Четыре, а не «сколько
/// угодно»: хабов мы не умеем, значит устройств ровно столько, сколько корневых портов занято
/// руками человека.
const MAX_DEVS: usize = 4;

/// Перечисленное устройство: слот, его EP0 и буфер под дескрипторы. Всё, что нужно, чтобы
/// говорить с устройством управляющими трансферами; классовые эндпоинты живут отдельно.
#[derive(Clone, Copy)]
struct Dev {
    slot: u8,
    port: u32,
    speed: u32,
    ep0_ring: usize, // TR-кольцо управляющего эндпоинта EP0
    ep0_enq: usize,
    ep0_cycle: u32,
    dma_buf: usize, // буфер под дескрипторы и данные control-трансферов (один фрейм, DMA)
}

pub struct Xhci {
    // База операционных регистров и число портов живут только на подъёме (локальными): после
    // перечисления мы к портам не возвращаемся — горячего подключения у нас нет, устройство
    // опознаётся один раз при загрузке. Заводить поля «на будущее» значит держать в структуре
    // то, чего никто не читает.
    db: usize, // база массива дверных звонков
    rt: usize, // база runtime-регистров
    ctx64: bool, // размер контекста 64 Б (иначе 32)
    cmd_ring: usize, // кольцо команд (RING_TRBS × 16 Б, последний — Link)
    cmd_enq: usize, // индекс постановки команды
    cmd_cycle: u32, // producer cycle state кольца команд
    event_ring: usize,
    event_deq: usize,
    event_cycle: u32, // consumer cycle state кольца событий
    dcbaa: usize, // массив базовых адресов контекстов устройств
    // Часть B — перечисленные устройства (Веха 196: до MAX_DEVS вместо одного).
    devs: [Option<Dev>; MAX_DEVS],
    // Часть C — HID-клавиатура (interrupt IN эндпоинт):
    kbd: Option<usize>, // индекс устройства-клавиатуры в `devs`
    int_ring: usize, // TR-кольцо interrupt-эндпоинта (0 — не настроен)
    int_enq: usize,
    int_cycle: u32,
    int_dci: u32, // Device Context Index interrupt-эндпоинта (звонок)
    int_buf: usize, // фрейм под boot-репорты: [`KBD_REPORTS`] буферов по 8 байт
    int_slot: usize, // какой буфер читать следующим (завершения приходят по порядку)
    prev: [u8; 6], // предыдущий набор нажатых клавиш (для детекта НОВЫХ нажатий)
    // Веха 196 — накопитель (bulk IN/OUT, протокол BOT поверх SCSI):
    msc: Option<Msc>,
}

/// USB-накопитель: два bulk-эндпоинта и то, что о нём рассказал SCSI.
#[derive(Clone, Copy)]
struct Msc {
    dev: usize,      // индекс устройства в `devs`
    in_ring: usize,  // TR-кольцо bulk-IN
    in_enq: usize,
    in_cycle: u32,
    in_dci: u32,
    out_ring: usize, // TR-кольцо bulk-OUT
    out_enq: usize,
    out_cycle: u32,
    out_dci: u32,
    buf: usize,      // общий DMA-буфер (CBW/CSW и один сектор)
    tag: u32,        // счётчик меток CBW: ответ обязан прийти с той же
    blocks: u64,     // ёмкость в блоках (из READ CAPACITY)
    block_len: u32,  // размер блока, байт
    base: u64,       // LBA начала store: 0 — раздела VOID на флешке нет
    capacity: u64,   // ёмкость раздела store в секторах
}
unsafe impl Send for Xhci {}

static XHCI: SpinLock<Option<Xhci>> = SpinLock::new(None);

/// Веха 87 — доступ к структуре по её ФИЗИЧЕСКОМУ адресу через direct-map. Кольца, контексты и
/// DMA-буферы xHCI живут по физическим адресам (их читает контроллер), а ядро ходит по ним так.
#[inline(always)]
fn dm(pa: usize) -> *mut u8 {
    crate::frame::ptr(pa)
}

#[inline]
unsafe fn rd(a: usize) -> u32 {
    read_volatile(a as *const u32)
}
#[inline]
unsafe fn wr(a: usize, v: u32) {
    write_volatile(a as *mut u32, v);
}
/// 64-битный регистр xHCI — двумя 32-битными записями (низ, затем верх).
#[inline]
unsafe fn wr64(a: usize, v: u64) {
    write_volatile(a as *mut u32, v as u32);
    write_volatile((a + 4) as *mut u32, (v >> 32) as u32);
}

/// Часть A — поднять контроллер xHCI, если он есть. `true` — кольца работают (проверено
/// командой Enable Slot). Состояние сохраняется для частей B/C.
pub fn init() -> bool {
    // Веха 199.24 — ОТКАЗ НАЗЫВАЕТ СЕБЯ. Здесь стояло молчаливое `return false`, и на машине
    // владельца это стоило захода: в описи шины ASMedia xHCI ВИДЕН (`03:00.0 1b21:1042`, класс
    // 0c0330), а от драйвера в журнале не было ни строки — ни «нашёл», ни «не нашёл». Отличить
    // «контроллера нет» от «нашёл и сломался на первом шаге» было нечем.
    let Some(base) = crate::arch::probe_xhci() else {
        crate::println!("  [usb]  xHCI: контроллера на шине не нашлось (класс 0c/03/30)");
        return false;
    };
    crate::println!("  [usb]  xHCI: контроллер {:#x} — поднимаю", base);
    unsafe {
        let took = bios_handoff(base);
        let caplen = (rd(base + CAP_CAPLENGTH) & 0xff) as usize;
        let op = base + caplen;
        let db = base + (rd(base + CAP_DBOFF) & !0x3) as usize;
        let rt = base + (rd(base + CAP_RTSOFF) & !0x1f) as usize;
        let hcs1 = rd(base + CAP_HCSPARAMS1);
        let max_slots = hcs1 & 0xff;
        let max_ports = (hcs1 >> 24) & 0xff;
        let ctx64 = rd(base + CAP_HCCPARAMS1) & (1 << 2) != 0;

        // 1) Остановить и сбросить контроллер; дождаться готовности (CNR=0) и конца сброса.
        wr(op + OP_USBCMD, rd(op + OP_USBCMD) & !CMD_RS);
        for _ in 0..1_000_000 {
            if rd(op + OP_USBSTS) & STS_HCH != 0 {
                break;
            }
        }
        wr(op + OP_USBCMD, CMD_HCRST);
        for _ in 0..1_000_000 {
            if rd(op + OP_USBCMD) & CMD_HCRST == 0 && rd(op + OP_USBSTS) & STS_CNR == 0 {
                break;
            }
        }

        // 2) Разрешить слоты (MaxSlotsEn = MaxSlots).
        wr(op + OP_CONFIG, max_slots);

        // 3) DCBAA — массив базовых адресов контекстов (обнулён; scratchpad у QEMU 0).
        let Some(dcbaa) = frame::alloc() else {
            crate::println!("  [usb]  xHCI: не хватило памяти под массив контекстов");
            return false;
        };
        wr64(op + OP_DCBAAP, dcbaa as u64);

        // 4) Кольцо команд: Link TRB в конце заворачивает на начало (Toggle Cycle).
        let Some(cmd_ring) = frame::alloc() else {
            crate::println!("  [usb]  xHCI: не хватило памяти под кольцо команд");
            return false;
        };
        let link = dm(cmd_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, cmd_ring as u64); // указатель назад на старт
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1); // тип Link | Toggle | Cycle
        wr64(op + OP_CRCR, cmd_ring as u64 | 1); // RCS=1

        // 5) Кольцо событий + таблица сегментов (ERST, 1 сегмент).
        let Some(event_ring) = frame::alloc() else {
            crate::println!("  [usb]  xHCI: не хватило памяти под кольцо событий");
            return false;
        };
        let Some(erst) = frame::alloc() else {
            crate::println!("  [usb]  xHCI: не хватило памяти под таблицу кольца событий");
            return false;
        };
        write_volatile(dm(erst) as *mut u64, event_ring as u64); // база сегмента
        write_volatile(dm(erst + 8) as *mut u32, RING_TRBS as u32); // размер сегмента (TRB)
        wr(rt + 0x20 + IR0_ERSTSZ, 1); // один сегмент
        wr64(rt + 0x20 + IR0_ERDP, event_ring as u64); // указатель извлечения = старт
        wr64(rt + 0x20 + IR0_ERSTBA, erst as u64); // база таблицы (после ERDP — так велит спека)

        // 6) Запуск.
        wr(op + OP_USBCMD, rd(op + OP_USBCMD) | CMD_RS);
        for _ in 0..1_000_000 {
            if rd(op + OP_USBSTS) & STS_HCH == 0 {
                break;
            }
        }

        let mut x = Xhci {
            db, rt, ctx64,
            cmd_ring, cmd_enq: 0, cmd_cycle: 1,
            event_ring, event_deq: 0, event_cycle: 1,
            dcbaa,
            devs: [None; MAX_DEVS],
            kbd: None,
            int_ring: 0, int_enq: 0, int_cycle: 1, int_dci: 0, int_buf: 0, int_slot: 0,
            prev: [0; 6],
            msc: None,
        };

        // Порты: сбросить подключённые и перечислить ВСЕ, а не первый попавшийся (Веха 196).
        // Раньше драйвер брал первый порт с устройством и на этом заканчивал — значит
        // клавиатура и флешка исключали друг друга, и какая из них будет работать, решал
        // порядок портов. На машине человека так не бывает: там воткнуто и то и другое.
        let mut connected = 0u32;
        for p in 1..=max_ports {
            let psc = op + OP_PORTS + (p as usize - 1) * 0x10;
            let v = rd(psc);
            if v & PORTSC_CCS == 0 {
                continue;
            }
            connected += 1;
            // Сброс порта: PR=1, сохранив CCS/PP, не трогая RW1C-изменения. Ждём включения
            // ПО ВРЕМЕНИ: на железе сброс занимает десятки миллисекунд.
            wr(psc, v & !PORTSC_CHANGES | PORTSC_PR);
            for _ in 0..40 {
                if rd(psc) & PORTSC_PED != 0 {
                    break;
                }
                wait_ms(5);
            }
            wr(psc, rd(psc) & !PORTSC_CHANGES | PORTSC_CHANGES); // сбросить биты-изменения
            // ── Веха 199.25 — КАЖДЫЙ ОТКАЗ НАЗЫВАЕТ СЕБЯ ──
            //
            // Все четыре выхода отсюда были молчаливыми `continue`, и на машине владельца это
            // выглядело так: «xHCI: контроллер 0xdde00000 — поднимаю», и больше НИ СЛОВА. Порты
            // подключены (иначе напечаталось бы «ничего не подключено»), а перечислить не удалось
            // ни одного — и на каком шаге, не видно. Шаги значат разное: порт не включился после
            // сброса это одно, контроллер не дал слот — другое, устройство не приняло адрес —
            // третье. Искать их надо в разных местах.
            if rd(psc) & PORTSC_PED == 0 {
                // Веха 199.26 — СЫРОЙ PORTSC до и после. «Не включился» не отвечает, почему:
                // у xHCI один физический разъём представлен ДВУМЯ портами (USB 2 и USB 3), и
                // сброс SuperSpeed-порта, к которому подключено устройство USB 2, не поднимет
                // его никогда. Различить это можно только по скорости и состоянию линии, а они
                // в этом слове: биты 10..13 — скорость, 5..8 — состояние линии, 9 — питание.
                let after = rd(psc);
                crate::println!(
                    "  [usb]  xHCI: порт {} не включился после сброса (было {:#010x}, стало {:#010x};                     скорость {}, линия {}, питание {})",
                    p, v, after,
                    (after >> 10) & 0xf,
                    (after >> 5) & 0xf,
                    if after & (1 << 9) != 0 { "есть" } else { "НЕТ" },
                );
                continue; // порт не включился — устройства на нём для нас нет
            }
            let speed = (rd(psc) >> 10) & 0xf; // Port Speed [13:10]
            let Some(slot) = x.enable_slot() else {
                crate::println!("  [usb]  xHCI: порт {} — контроллер не дал слот", p);
                continue;
            };
            let Some(di) = x.address_device(slot, p, speed) else {
                crate::println!(
                    "  [usb]  xHCI: порт {} (скорость {}) — адрес выдать не удалось (slot {})",
                    p, speed, slot,
                );
                continue;
            };
            let mut desc = [0u8; 18];
            if !x.get_descriptor(di, 1, 0, &mut desc) {
                crate::println!(
                    "  [usb]  xHCI: порт {} — адрес выдан, а дескриптор не читается",
                    p,
                );
                continue;
            }
            let vid = desc[8] as u16 | (desc[9] as u16) << 8;
            let pid = desc[10] as u16 | (desc[11] as u16) << 8;
            // Класс объявлен либо у устройства, либо (чаще) у интерфейса — поэтому решает
            // разбор config-дескриптора, а не байт `bDeviceClass`.
            if x.kbd.is_none() && x.setup_hid(di) {
                crate::println!(
                    "  [usb]  xHCI: HID-клавиатура {:04x}:{:04x} на порту {} (slot {})",
                    vid, pid, p, slot,
                );
            } else if x.msc.is_none() && x.setup_msc(di) {
                let m = x.msc.expect("только что настроен");
                crate::println!(
                    "  [usb]  xHCI: накопитель {:04x}:{:04x} на порту {} — {} блоков по {} Б ({} МиБ)",
                    vid, pid, p, m.blocks, m.block_len,
                    m.blocks * m.block_len as u64 / (1024 * 1024),
                );
            } else {
                crate::println!(
                    "  [usb]  xHCI: устройство {:04x}:{:04x} на порту {} — не наш класс (нет драйвера)",
                    vid, pid, p,
                );
            }
        }
        if connected == 0 {
            crate::println!("  [usb]  xHCI: {} портов, ничего не подключено", max_ports);
        } else if x.kbd.is_none() && x.msc.is_none() {
            // Веха 199.25 — ИТОГ. Строки выше говорят про каждый порт отдельно, а здесь видно
            // главное одним взглядом: устройства есть, ни одно не наше.
            crate::println!(
                "  [usb]  xHCI: портов {}, подключено {}, перечислить не удалось ни одного",
                max_ports, connected,
            );
        }
        // Веха 199.1 — ничего нашего на контроллере нет: вернуть его прошивке. Пока он у неё,
        // она эмулирует USB-клавиатуру через контроллер 8042, и на машине, где наш драйвер не
        // справился, это единственный способ ввода. Тот же довод, что у EHCI: забрать и не дать
        // взамен ничего — худшее из возможного.
        if took && x.kbd.is_none() && x.msc.is_none() {
            release_to_bios(base, op);
            return false;
        }

        *XHCI.lock_irq() = Some(x);
    }
    true
}

/// Где у xHCI лежит «кому принадлежит контроллер» (расширенная возможность номер 1).
///
/// В отличие от EHCI, список возможностей у xHCI живёт не в конфигурации PCI, а в самих
/// регистрах: смещение первой — в старшей половине `HCCPARAMS1`, дальше список.
unsafe fn legsup_offset(base: usize) -> Option<usize> {
    let mut off = ((rd(base + CAP_HCCPARAMS1) >> 16) & 0xffff) as usize * 4;
    if off == 0 {
        return None;
    }
    for _ in 0..64 {
        let cap = rd(base + off);
        if cap & 0xff == 1 {
            return Some(off);
        }
        let next = ((cap >> 8) & 0xff) as usize * 4;
        if next == 0 {
            return None;
        }
        off += next;
    }
    None
}

const OS_OWNED: u32 = 1 << 24;
const BIOS_OWNED: u32 = 1 << 16;

/// Отобрать управление у прошивки (Веха 199.1).
///
/// Этого шага у нас не было ВОВСЕ — и в QEMU он не нужен, потому что там прошивка контроллером
/// не владеет. На живой машине владеет: пока её бит стоит, она обслуживает контроллер из
/// системного режима, и два владельца у одного устройства — это состязание, в котором
/// проигрывают оба.
unsafe fn bios_handoff(base: usize) -> bool {
    let Some(off) = legsup_offset(base) else { return false };
    let legsup = rd(base + off);
    if legsup & BIOS_OWNED == 0 {
        return false;
    }
    wr(base + off, legsup | OS_OWNED);
    for _ in 0..200 {
        if rd(base + off) & BIOS_OWNED == 0 {
            crate::println!("  [usb]  xHCI: управление отобрано у прошивки");
            return true;
        }
        wait_ms(5);
    }
    crate::println!("  [usb]  xHCI: прошивка не отдала управление — работаем всё равно");
    true
}

/// Вернуть контроллер прошивке: остановить и снять свой бит владения.
unsafe fn release_to_bios(base: usize, op: usize) {
    wr(op + OP_USBCMD, rd(op + OP_USBCMD) & !CMD_RS);
    for _ in 0..100 {
        if rd(op + OP_USBSTS) & STS_HCH != 0 {
            break;
        }
        wait_ms(5);
    }
    if let Some(off) = legsup_offset(base) {
        wr(base + off, rd(base + off) & !OS_OWNED);
    }
    crate::println!("  [usb]  xHCI: своих устройств нет — управление возвращено прошивке");
}

/// Подождать `ms` миллисекунд по измеренной таймбазе (Веха 199.1).
///
/// Здесь стояли холостые обороты («покрутиться миллион раз»), и на живом железе это значит
/// «почти не ждать»: сброс порта USB занимает десятки миллисекунд, а устройство после подачи
/// питания определяется за сотню. В эмуляторе всё происходит мгновенно, поэтому разницы не было
/// видно — ровно до первой настоящей машины.
fn wait_ms(ms: u64) {
    let until = crate::clock::uptime_ns() + ms * 1_000_000;
    while crate::clock::uptime_ns() < until {
        core::hint::spin_loop();
    }
}

/// Поставить TRB в кольцо передачи и продвинуть постановку — с ЧЕСТНЫМ заворотом (Веха 196).
///
/// Здесь была ошибка, общая для всех наших колец, и она не видна на коротких прогонах: при
/// заворачивании мы меняли СВОЙ бит цикла, а Link-TRB в конце кольца оставляли с тем, что
/// записали при создании. После первого круга его бит перестаёт совпадать с ожидаемым, и
/// контроллер честно останавливается на нём, считая кольцо пустым.
///
/// Клавиатуре это стоило бы восьмидесяти нажатий, и заметить было негде; накопителю — ста
/// двадцати семи секторов, и установка на флешку падала ровно на этом: «сбой записи
/// загрузочного префикса» после первых мегабайт.
///
/// Правило спецификации: producer пишет в Link-TRB свой ТЕКУЩИЙ бит цикла перед тем, как
/// перескочить через него, а бит Toggle Cycle (бит1) велит контроллеру перевернуть свой.
unsafe fn ring_push(
    ring: usize, enq: &mut usize, cycle: &mut u32,
    p_lo: u32, p_hi: u32, status: u32, control: u32,
) {
    let trb = dm(ring + *enq * 16) as *mut u32;
    write_volatile(trb, p_lo);
    write_volatile(trb.add(1), p_hi);
    write_volatile(trb.add(2), status);
    compiler_fence(Ordering::SeqCst);
    write_volatile(trb.add(3), control | *cycle);
    *enq += 1;
    if *enq == RING_TRBS - 1 {
        // Дошли до Link-TRB: отдать его контроллеру с нашим текущим циклом и перевернуть свой.
        let link = dm(ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, ring as u64);
        compiler_fence(Ordering::SeqCst);
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | *cycle);
        *enq = 0;
        *cycle ^= 1;
    }
}

impl Xhci {
    /// Поставить TRB в кольцо команд, позвонить в дверной звонок 0, дождаться Command
    /// Completion Event. Возвращает `[param_lo, param_hi, status, control]` события.
    unsafe fn command(&mut self, p_lo: u32, p_hi: u32, control: u32) -> Option<[u32; 4]> {
        let trb = dm(self.cmd_ring + self.cmd_enq * 16) as *mut u32;
        write_volatile(trb, p_lo);
        write_volatile(trb.add(1), p_hi);
        write_volatile(trb.add(2), 0);
        write_volatile(trb.add(3), control | self.cmd_cycle);
        compiler_fence(Ordering::SeqCst);
        // Продвинуть постановку; предпоследний слот — перед Link, поэтому заворот на 0.
        self.cmd_enq += 1;
        if self.cmd_enq == RING_TRBS - 1 {
            self.cmd_enq = 0;
            self.cmd_cycle ^= 1;
        }
        wr(self.db, 0); // дверной звонок 0 (host controller command ring)
        // Дождаться именно Command Completion Event, пропуская попутные (Port Status Change от
        // сброса портов и т.п.) — они кладутся в то же кольцо событий.
        for _ in 0..64 {
            let ev = self.wait_event()?;
            if (ev[3] >> 10) & 0x3f == EV_CMD_COMPLETE {
                return Some(ev);
            }
        }
        None
    }

    /// Не-блокирующе взять одно событие, если оно готово (совпал cycle bit), продвинуть ERDP.
    unsafe fn try_event(&mut self) -> Option<[u32; 4]> {
        let ev = dm(self.event_ring + self.event_deq * 16) as *const u32;
        let ctrl = read_volatile(ev.add(3));
        if ctrl & 1 != self.event_cycle {
            return None;
        }
        let out = [read_volatile(ev), read_volatile(ev.add(1)), read_volatile(ev.add(2)), ctrl];
        self.event_deq += 1;
        if self.event_deq == RING_TRBS {
            self.event_deq = 0;
            self.event_cycle ^= 1;
        }
        let erdp = self.event_ring + self.event_deq * 16;
        wr64(self.rt + 0x20 + IR0_ERDP, erdp as u64 | ERDP_EHB);
        Some(out)
    }

    /// Дождаться (с таймаутом) любого события — для команд/трансферов при инициализации.
    unsafe fn wait_event(&mut self) -> Option<[u32; 4]> {
        for _ in 0..10_000_000 {
            if let Some(ev) = self.try_event() {
                return Some(ev);
            }
        }
        None
    }

    /// Enable Slot: выделить слот устройства. Возвращает slot id (из Command Completion Event,
    /// если код завершения = 1 «успех»).
    unsafe fn enable_slot(&mut self) -> Option<u8> {
        // command гарантирует Command Completion Event; код завершения в status[31:24]
        // (1 = успех), slot id в control[31:24].
        let ev = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        ((ev[2] >> 24) & 0xff == 1).then(|| (ev[3] >> 24) as u8)
    }

    /// Address Device (Веха 50 B): собрать input-контекст (slot + EP0), выделить контекст
    /// устройства в DCBAA[slot] и TR-кольцо EP0, выдать команду. Возвращает ИНДЕКС устройства
    /// в [`Xhci::devs`] — по нему дальше идут все управляющие трансферы (Веха 196: устройств
    /// несколько, и «текущего» больше нет).
    unsafe fn address_device(&mut self, slot: u8, port: u32, speed: u32) -> Option<usize> {
        let cs = if self.ctx64 { 64 } else { 32 };
        let Some(di) = (0..MAX_DEVS).find(|&i| self.devs[i].is_none()) else {
            return None; // мест больше нет — остальные порты не перечисляем
        };
        let (Some(dev_ctx), Some(ep0_ring), Some(input), Some(dma)) =
            (frame::alloc(), frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return None;
        };
        write_volatile(dm(self.dcbaa + slot as usize * 8) as *mut u64, dev_ctx as u64);
        // TR-кольцо EP0 с Link-заворотом.
        let link = dm(ep0_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, ep0_ring as u64);
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1);
        self.devs[di] = Some(Dev {
            slot, port, speed, ep0_ring, ep0_enq: 0, ep0_cycle: 1, dma_buf: dma,
        });
        // Input Control Context (0): Add flags A0 (slot) | A1 (EP0).
        write_volatile(dm(input + 4) as *mut u32, 0b11);
        // Slot Context (1): Context Entries=1, Speed; Root Hub Port Number.
        let sc = input + cs;
        write_volatile(dm(sc) as *mut u32, 1 << 27 | speed << 20);
        // Веха 173 — `dm()` здесь ОБЯЗАТЕЛЕН, и его тут не было. `sc` — физический адрес кадра, а
        // с Вехи 87 ядро живёт в верхней половине: прямая карта смещена, и физический адрес,
        // взятый как указатель, не отображён никуда. Строка писала по адресу вида `0x1753024` и
        // валила ядро page fault'ом — то есть машина с xHCI и любым USB-устройством не
        // загружалась вовсе. Драйвер писался до переезда ядра (Веха 50), а поймать это было
        // некому: в QEMU xHCI-устройств у нас на стенде не было, а у владельца ноутбук на EHCI.
        write_volatile(dm(sc + 4) as *mut u32, port << 16);
        // EP0 Context (2): MPS по скорости, EPType=Control(4), CErr=3; TR dequeue|DCS; avg TRB=8.
        let ep = input + 2 * cs;
        let mps: u32 = match speed { 3 => 64, 4 => 512, _ => 8 };
        write_volatile(dm(ep + 4) as *mut u32, mps << 16 | 4 << 3 | 3 << 1);
        write_volatile(dm(ep + 8) as *mut u64, ep0_ring as u64 | 1);
        write_volatile(dm(ep + 16) as *mut u32, 8);
        compiler_fence(Ordering::SeqCst);
        let ev = self.command(input as u32, (input as u64 >> 32) as u32,
            TRB_ADDR_DEV << 10 | (slot as u32) << 24);
        if matches!(ev, Some(e) if (e[2] >> 24) & 0xff == 1) {
            Some(di)
        } else {
            self.devs[di] = None; // адрес не выдан — запись устройства не оставляем
            None
        }
    }

    /// Поставить TRB в TR-кольцо EP0 устройства `di` (с заворотом на Link).
    unsafe fn push_ep0(&mut self, di: usize, p_lo: u32, p_hi: u32, status: u32, control: u32) {
        let Some(d) = self.devs[di].as_mut() else { return };
        ring_push(d.ep0_ring, &mut d.ep0_enq, &mut d.ep0_cycle, p_lo, p_hi, status, control);
    }

    /// Управляющий IN-трансфер по EP0: Setup + Data(IN) + Status(OUT, IOC), звонок EP0, ждём
    /// Transfer Event. Данные приходят в [`Self::dma_buf`]. `true` — успех/короткий пакет.
    unsafe fn control_in(&mut self, di: usize, req_type: u8, request: u8, value: u16, index: u16, len: u16) -> bool {
        let Some(d) = self.devs[di] else { return false };
        let setup = req_type as u64 | (request as u64) << 8 | (value as u64) << 16
            | (index as u64) << 32 | (len as u64) << 48;
        // Setup Stage(2): IDT(бит6), TRT=IN(3) в [17:16].
        self.push_ep0(di, setup as u32, (setup >> 32) as u32, 8, TRB_SETUP << 10 | 3 << 16 | 1 << 6);
        // Data Stage(3): DIR=IN(бит16).
        self.push_ep0(di, d.dma_buf as u32, (d.dma_buf as u64 >> 32) as u32, len as u32,
            TRB_DATA << 10 | 1 << 16);
        // Status Stage(4): DIR=OUT, IOC(бит5).
        self.push_ep0(di, 0, 0, 0, TRB_STATUS << 10 | 1 << 5);
        compiler_fence(Ordering::SeqCst);
        wr(self.db + d.slot as usize * 4, 1); // звонок EP0 (DCI 1)
        for _ in 0..64 {
            let Some(ev) = self.wait_event() else { return false };
            if (ev[3] >> 10) & 0x3f == EV_TRANSFER {
                let code = (ev[2] >> 24) & 0xff;
                return code == 1 || code == 13; // успех или короткий пакет
            }
        }
        false
    }

    /// GET_DESCRIPTOR по EP0 устройства `di` → скопировать `out.len()` байт из его DMA-буфера.
    unsafe fn get_descriptor(&mut self, di: usize, dtype: u8, index: u8, out: &mut [u8]) -> bool {
        let value = (dtype as u16) << 8 | index as u16;
        if !self.control_in(di, 0x80, 6, value, 0, out.len() as u16) {
            return false;
        }
        let Some(d) = self.devs[di] else { return false };
        core::ptr::copy_nonoverlapping(dm(d.dma_buf) as *const u8, out.as_mut_ptr(), out.len());
        true
    }

    /// Управляющий трансфер БЕЗ данных (SET_CONFIGURATION, SET_PROTOCOL): Setup + Status(IN,IOC).
    unsafe fn control_nodata(&mut self, di: usize, req_type: u8, request: u8, value: u16, index: u16) -> bool {
        let Some(d) = self.devs[di] else { return false };
        let setup = req_type as u64 | (request as u64) << 8 | (value as u64) << 16
            | (index as u64) << 32; // wLength=0
        self.push_ep0(di, setup as u32, (setup >> 32) as u32, 8, TRB_SETUP << 10 | 1 << 6); // TRT=No Data
        self.push_ep0(di, 0, 0, 0, TRB_STATUS << 10 | 1 << 16 | 1 << 5); // Status DIR=IN, IOC
        compiler_fence(Ordering::SeqCst);
        wr(self.db + d.slot as usize * 4, 1);
        for _ in 0..64 {
            let Some(ev) = self.wait_event() else { return false };
            if (ev[3] >> 10) & 0x3f == EV_TRANSFER {
                let c = (ev[2] >> 24) & 0xff;
                return c == 1 || c == 13;
            }
        }
        false
    }

    /// Часть C — настроить HID-клавиатуру: разобрать config-дескриптор (найти interrupt-IN
    /// эндпоинт + интерфейс), SET_CONFIGURATION, SET_PROTOCOL(boot), Configure Endpoint, поставить
    /// первый interrupt-TRB. `true` — это HID-клавиатура и она готова слать репорты.
    unsafe fn setup_hid(&mut self, di: usize) -> bool {
        let Some(speed) = self.devs[di].map(|d| d.speed) else { return false };
        let mut cfg = [0u8; 96];
        if !self.get_descriptor(di, 2, 0, &mut cfg) {
            return false; // config-дескриптор (тип 2)
        }
        // Пройти дескрипторы: интерфейс (тип 4, класс[5]=3 HID) + его interrupt-IN эндпоинт (тип 5).
        let (mut iface, mut is_hid, mut ep_addr, mut ep_mps, mut ep_ivl) = (0u8, false, 0u8, 8u16, 0u8);
        let mut i = cfg[0] as usize; // после шапки config
        while i + 4 <= cfg.len() && cfg[i] != 0 {
            let (blen, btype) = (cfg[i] as usize, cfg[i + 1]);
            if btype == 4 {
                iface = cfg[i + 2];
                is_hid = cfg[i + 5] == 3; // bInterfaceClass = HID
            } else if btype == 5 && is_hid && cfg[i + 2] & 0x80 != 0 && cfg[i + 3] & 3 == 3 {
                // Endpoint: IN (бит7 адреса) + Interrupt (атрибуты[1:0]=3).
                ep_addr = cfg[i + 2];
                ep_mps = cfg[i + 4] as u16 | (cfg[i + 5] as u16) << 8;
                ep_ivl = cfg[i + 6];
            }
            i += blen.max(1);
        }
        if ep_addr == 0 {
            return false; // не HID с interrupt-IN эндпоинтом
        }
        // SET_CONFIGURATION(1); SET_PROTOCOL(boot=0) на интерфейс.
        self.control_nodata(di, 0x00, 9, 1, 0);
        self.control_nodata(di, 0x21, 0x0b, 0, iface as u16);
        self.configure_endpoint(di, ep_addr, ep_mps, ep_ivl, speed) && {
            self.kbd = Some(di);
            self.int_slot = 0;
            for slot in 0..KBD_REPORTS {
                self.queue_report_at(slot); // все буферы сразу — иначе нажатия теряются
            }
            true
        }
    }

    /// Configure Endpoint: добавить interrupt-IN эндпоинт в контекст устройства.
    unsafe fn configure_endpoint(&mut self, di: usize, ep_addr: u8, mps: u16, ivl: u8, speed: u32) -> bool {
        let Some(d) = self.devs[di] else { return false };
        let cs = if self.ctx64 { 64 } else { 32 };
        let dci = 2 * (ep_addr & 0x0f) as u32 + 1; // IN-эндпоинт: DCI = 2*n+1
        let (Some(int_ring), Some(input), Some(int_buf)) =
            (frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return false;
        };
        let link = dm(int_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, int_ring as u64);
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1);
        self.int_ring = int_ring;
        self.int_enq = 0;
        self.int_cycle = 1;
        self.int_dci = dci;
        self.int_buf = int_buf;
        // Input Control: A0 (slot) | A(dci). Slot Context: Context Entries = dci.
        write_volatile(dm(input + 4) as *mut u32, 1 | 1 << dci);
        write_volatile(dm(input + cs) as *mut u32, dci << 27 | speed << 20);
        // EP Context (индекс dci+1): interval; EPType=Interrupt IN(7), MPS, CErr=3; TR dequeue|DCS.
        let ep = input + (dci as usize + 1) * cs;
        let interval = if speed >= 3 { (ivl.max(1) - 1).min(15) as u32 } else { 7 };
        write_volatile(dm(ep) as *mut u32, interval << 16);
        write_volatile(dm(ep + 4) as *mut u32, (mps as u32) << 16 | 7 << 3 | 3 << 1);
        write_volatile(dm(ep + 8) as *mut u64, int_ring as u64 | 1);
        write_volatile(dm(ep + 16) as *mut u32, mps as u32); // avg TRB length
        compiler_fence(Ordering::SeqCst);
        let ev = self.command(input as u32, (input as u64 >> 32) as u32,
            TRB_CONFIG_EP << 10 | (d.slot as u32) << 24);
        matches!(ev, Some(e) if (e[2] >> 24) & 0xff == 1)
    }

    /// Поставить Normal-TRB на interrupt-кольцо (приём одного 8-байтного репорта в буфер
    /// `slot`) + звонок.
    unsafe fn queue_report_at(&mut self, slot: usize) {
        let (ring, buf) = (self.int_ring, self.int_buf + slot * 8);
        ring_push(
            ring, &mut self.int_enq, &mut self.int_cycle,
            buf as u32, (buf as u64 >> 32) as u32, 8,
            TRB_NORMAL << 10 | 1 << 5 | 1 << 2, // IOC | ISP
        );
        compiler_fence(Ordering::SeqCst);
        let Some(ki) = self.kbd else { return };
        let Some(d) = self.devs[ki] else { return };
        wr(self.db + d.slot as usize * 4, self.int_dci); // звонок interrupt-эндпоинта
    }

    // ─── накопитель: BOT поверх SCSI (Веха 196) ────────────────────────────────────────────
    //
    // Почему BOT, а не UAS. Bulk-Only Transport — это три пакета на команду: CBW (31 байт
    // наружу), данные, CSW (13 байт внутрь). Ни очередей, ни тегов задач, ни потоков (streams)
    // — то есть ровно то, что нужно store'у, который читает и пишет сектор за сектором
    // синхронно. UAS быстрее на глубокой очереди, но требует streams в xHCI и очередь команд,
    // а это подсистема ради выигрыша, которого у нас пока негде получить.

    /// Разобрать config-дескриптор и, если это накопитель (класс 8, SCSI, BOT), поднять его:
    /// SET_CONFIGURATION, два bulk-эндпоинта в контекст устройства, затем SCSI-опрос ёмкости.
    unsafe fn setup_msc(&mut self, di: usize) -> bool {
        let Some(speed) = self.devs[di].map(|d| d.speed) else { return false };
        let mut cfg = [0u8; 96];
        if !self.get_descriptor(di, 2, 0, &mut cfg) {
            return false;
        }
        // Интерфейс класса 8 (mass storage), подкласс 6 (SCSI прозрачный), протокол 0x50 (BOT)
        // и два его bulk-эндпоинта. Другие подклассы (UFI, RBC) и UAS не берём: у них другой
        // набор команд, и делать вид, что мы их понимаем, значит портить чужой диск.
        let (mut ok, mut in_addr, mut out_addr, mut in_mps, mut out_mps) = (false, 0u8, 0u8, 512u16, 512u16);
        let mut i = cfg[0] as usize;
        while i + 4 <= cfg.len() && cfg[i] != 0 {
            let (blen, btype) = (cfg[i] as usize, cfg[i + 1]);
            if btype == 4 {
                ok = cfg[i + 5] == 8 && cfg[i + 6] == 6 && cfg[i + 7] == 0x50;
            } else if btype == 5 && ok && cfg[i + 3] & 3 == 2 {
                // Endpoint, атрибуты[1:0] = 2 (bulk); бит7 адреса — направление.
                let mps = cfg[i + 4] as u16 | (cfg[i + 5] as u16) << 8;
                if cfg[i + 2] & 0x80 != 0 {
                    in_addr = cfg[i + 2];
                    in_mps = mps;
                } else {
                    out_addr = cfg[i + 2];
                    out_mps = mps;
                }
            }
            i += blen.max(1);
        }
        if !ok || in_addr == 0 || out_addr == 0 {
            return false;
        }
        self.control_nodata(di, 0x00, 9, 1, 0); // SET_CONFIGURATION(1)

        let (Some(in_ring), Some(out_ring), Some(buf)) =
            (frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return false;
        };
        for r in [in_ring, out_ring] {
            let link = dm(r + (RING_TRBS - 1) * 16) as *mut u32;
            write_volatile(link as *mut u64, r as u64);
            write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1);
        }
        let in_dci = 2 * (in_addr & 0x0f) as u32 + 1; // IN:  DCI = 2n+1
        let out_dci = 2 * (out_addr & 0x0f) as u32; // OUT: DCI = 2n
        if !self.configure_bulk(di, in_ring, in_dci, in_mps, out_ring, out_dci, out_mps, speed) {
            return false;
        }
        self.msc = Some(Msc {
            dev: di,
            in_ring, in_enq: 0, in_cycle: 1, in_dci,
            out_ring, out_enq: 0, out_cycle: 1, out_dci,
            buf, tag: 1, blocks: 0, block_len: 512, base: 0, capacity: 0,
        });
        // Накопитель после подъёма имеет право один раз ответить «я не готов» (это нормальная
        // часть спецификации, а не сбой), поэтому спрашиваем ёмкость с несколькими попытками.
        for _ in 0..8 {
            if self.read_capacity() {
                return true;
            }
        }
        self.msc = None;
        false
    }

    /// Configure Endpoint сразу для ДВУХ эндпоинтов: bulk-IN и bulk-OUT. Одной командой, а не
    /// двумя: контекст устройства объявляет число записей (`Context Entries`), и ставить его
    /// дважды значит переписывать уже настроенное.
    #[allow(clippy::too_many_arguments)]
    unsafe fn configure_bulk(
        &mut self, di: usize, in_ring: usize, in_dci: u32, in_mps: u16,
        out_ring: usize, out_dci: u32, out_mps: u16, speed: u32,
    ) -> bool {
        let Some(d) = self.devs[di] else { return false };
        let cs = if self.ctx64 { 64 } else { 32 };
        let Some(input) = frame::alloc() else { return false };
        let last = in_dci.max(out_dci);
        // Input Control: A0 (slot) | A(in) | A(out). Slot Context: Context Entries = последний DCI.
        write_volatile(dm(input + 4) as *mut u32, 1 | 1 << in_dci | 1 << out_dci);
        write_volatile(dm(input + cs) as *mut u32, last << 27 | speed << 20);
        write_volatile(dm(input + cs + 4) as *mut u32, d.port << 16);
        // EPType: Bulk OUT = 2, Bulk IN = 6. CErr=3, интервал не задаётся (bulk).
        for (dci, ring, mps, ep_type) in
            [(out_dci, out_ring, out_mps, 2u32), (in_dci, in_ring, in_mps, 6u32)]
        {
            let ep = input + (dci as usize + 1) * cs;
            write_volatile(dm(ep + 4) as *mut u32, (mps as u32) << 16 | ep_type << 3 | 3 << 1);
            write_volatile(dm(ep + 8) as *mut u64, ring as u64 | 1);
            write_volatile(dm(ep + 16) as *mut u32, mps as u32);
        }
        compiler_fence(Ordering::SeqCst);
        let ev = self.command(input as u32, (input as u64 >> 32) as u32,
            TRB_CONFIG_EP << 10 | (d.slot as u32) << 24);
        matches!(ev, Some(e) if (e[2] >> 24) & 0xff == 1)
    }

    /// Один bulk-трансфер: поставить Normal-TRB, позвонить, дождаться Transfer Event.
    /// Возвращает число НЕ переданных байт (residue) либо `None` при отказе.
    unsafe fn bulk(&mut self, dir_in: bool, pa: usize, len: u32) -> Option<u32> {
        let m = self.msc.as_mut()?;
        let (ring, enq, cycle, dci) = if dir_in {
            (m.in_ring, &mut m.in_enq, &mut m.in_cycle, m.in_dci)
        } else {
            (m.out_ring, &mut m.out_enq, &mut m.out_cycle, m.out_dci)
        };
        // IOC (бит5) — событие по завершении; ISP (бит2) — и на коротком пакете тоже: у SCSI
        // короткий ответ это норма, а не ошибка.
        ring_push(
            ring, enq, cycle, pa as u32, (pa as u64 >> 32) as u32, len,
            TRB_NORMAL << 10 | 1 << 5 | 1 << 2,
        );
        let dev = m.dev;
        let Some(d) = self.devs[dev] else { return None };
        compiler_fence(Ordering::SeqCst);
        wr(self.db + d.slot as usize * 4, dci);
        for _ in 0..64 {
            let ev = self.wait_event()?;
            if (ev[3] >> 10) & 0x3f != EV_TRANSFER {
                continue;
            }
            // Чужое событие не выбрасываем: репорт клавиатуры, попавший сюда, — это нажатие
            // человека, и потерять его значит «клавиатура иногда не работает».
            if self.is_kbd_event(&ev) {
                self.take_report();
                continue;
            }
            let (slot, ev_dci) = Self::ev_addr(&ev);
            if slot != d.slot || ev_dci != dci {
                continue; // не наш эндпоинт
            }
            let code = (ev[2] >> 24) & 0xff;
            if code == 1 || code == 13 {
                return Some(ev[2] & 0x00ff_ffff); // residue: сколько НЕ передано
            }
            return None;
        }
        None
    }

    /// Команда SCSI по правилам BOT: CBW → (данные) → CSW. `cmd` — блок команды (6/10 байт),
    /// `len` — сколько байт данных ожидается, `dir_in` — куда они идут.
    ///
    /// Буфер данных — тот же фрейм, что и CBW/CSW, но со смещением: пакеты транспорта короткие,
    /// а сектор ровно один, и отдельный фрейм под каждый значил бы три фрейма вместо одного.
    unsafe fn scsi(&mut self, cmd: &[u8], len: u32, dir_in: bool) -> bool {
        let Some(m) = self.msc else { return false };
        let (buf, tag) = (m.buf, m.tag);
        let data = buf + 64; // CBW занимает 31 байт, CSW — 13; данные кладём за ними
        // CBW: сигнатура 'USBC', метка, длина данных, флаги (бит7 = IN), LUN 0, длина команды.
        let cbw = dm(buf);
        core::ptr::write_bytes(cbw, 0, 31);
        core::ptr::copy_nonoverlapping(b"USBC".as_ptr(), cbw, 4);
        write_volatile(cbw.add(4) as *mut u32, tag);
        write_volatile(cbw.add(8) as *mut u32, len);
        write_volatile(cbw.add(12), if dir_in { 0x80 } else { 0x00 });
        write_volatile(cbw.add(13), 0); // LUN 0: несколько LUN на флешке не бывает
        write_volatile(cbw.add(14), cmd.len() as u8);
        core::ptr::copy_nonoverlapping(cmd.as_ptr(), cbw.add(15), cmd.len());
        compiler_fence(Ordering::SeqCst);
        if self.bulk(false, buf, 31).is_none() {
            return false;
        }
        if len > 0 && self.bulk(dir_in, data, len).is_none() {
            return false;
        }
        // CSW: сигнатура 'USBS', та же метка, статус (0 — команда выполнена).
        if self.bulk(true, buf + 32, 13).is_none() {
            return false;
        }
        let csw = dm(buf + 32);
        let sig = core::ptr::read_unaligned(csw as *const u32);
        let got_tag = core::ptr::read_unaligned(csw.add(4) as *const u32);
        let status = read_volatile(csw.add(12));
        if let Some(m) = self.msc.as_mut() {
            m.tag = m.tag.wrapping_add(1);
        }
        sig == u32::from_le_bytes(*b"USBS") && got_tag == tag && status == 0
    }

    /// READ CAPACITY(10): последний доступный блок и его размер. Отсюда берётся ёмкость.
    unsafe fn read_capacity(&mut self) -> bool {
        // TEST UNIT READY первым: свежеподнятый накопитель отвечает «не готов», пока не
        // раскрутится, и спрашивать ёмкость до этого бессмысленно.
        self.scsi(&[0x00, 0, 0, 0, 0, 0], 0, false);
        if !self.scsi(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8, true) {
            return false;
        }
        let Some(m) = self.msc else { return false };
        let p = dm(m.buf + 64);
        // Оба числа — big-endian: это SCSI, а не x86.
        let last = u32::from_be_bytes([
            read_volatile(p), read_volatile(p.add(1)),
            read_volatile(p.add(2)), read_volatile(p.add(3)),
        ]);
        let blen = u32::from_be_bytes([
            read_volatile(p.add(4)), read_volatile(p.add(5)),
            read_volatile(p.add(6)), read_volatile(p.add(7)),
        ]);
        if blen == 0 || last == 0 {
            return false;
        }
        if let Some(m) = self.msc.as_mut() {
            m.blocks = last as u64 + 1;
            m.block_len = blen;
        }
        true
    }

    /// READ(10)/WRITE(10) одного сектора. `sector` — абсолютный номер блока накопителя.
    unsafe fn rw_sector(&mut self, write: bool, sector: u64, buf: &mut [u8; SECTOR]) -> bool {
        let Some(m) = self.msc else { return false };
        if m.block_len as usize != SECTOR || sector >= m.blocks {
            return false;
        }
        let lba = sector as u32; // 32-битный LBA: READ(10) больше и не умеет (2 ТиБ)
        let data = m.buf + 64;
        if write {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dm(data), SECTOR);
        }
        let op = if write { 0x2a } else { 0x28 };
        let cmd = [
            op, 0,
            (lba >> 24) as u8, (lba >> 16) as u8, (lba >> 8) as u8, lba as u8,
            0, 0, 1, 0, // одна блока за раз
        ];
        if !self.scsi(&cmd, SECTOR as u32, !write) {
            return false;
        }
        if !write {
            core::ptr::copy_nonoverlapping(dm(data) as *const u8, buf.as_mut_ptr(), SECTOR);
        }
        true
    }

    /// Чей это Transfer Event: слот устройства и номер эндпоинта (DCI) из управляющего слова.
    ///
    /// Веха 196 — без этого разбора кольцо событий ОДНО на всех, и кто первый его читает, тот
    /// и забирает чужое. Накопитель, ожидая завершения своей передачи, съедал репорты
    /// клавиатуры — и она «умирала» ровно тогда, когда система живёт на флешке, то есть в
    /// единственном случае, ради которого веха и делалась.
    fn ev_addr(ev: &[u32; 4]) -> (u8, u32) {
        ((ev[3] >> 24) as u8, (ev[3] >> 16) & 0x1f)
    }

    /// Клавиатура ли это (её слот и её эндпоинт).
    fn is_kbd_event(&self, ev: &[u32; 4]) -> bool {
        let (slot, dci) = Self::ev_addr(ev);
        self.kbd
            .and_then(|ki| self.devs[ki])
            .is_some_and(|d| d.slot == slot && dci == self.int_dci)
    }

    /// Опрос: разобрать пришедшие boot-репорты клавиатуры, отдать НОВЫЕ нажатия в консоль.
    unsafe fn poll_hid(&mut self) {
        while let Some(ev) = self.try_event() {
            if (ev[3] >> 10) & 0x3f != EV_TRANSFER || !self.is_kbd_event(&ev) {
                continue; // не трансфер либо чужой эндпоинт — не наше дело
            }
            self.take_report();
        }
    }

    /// Разобрать один пришедший репорт клавиатуры и подставить буфер под следующий.
    unsafe fn take_report(&mut self) {
        // Разбор отчёта — общий с EHCI ([`crate::usb_hid`]): протокол один и тот же, а две
        // копии одного разбора разошлись бы на первой же правке раскладки.
        let r = core::slice::from_raw_parts(dm(self.int_buf + self.int_slot * 8) as *const u8, 8);
        crate::usb_hid::report(&mut self.prev, r);
        // Этот буфер свободен — вернуть его в кольцо и перейти к следующему по кругу:
        // завершения на одном эндпоинте приходят в том же порядке, в каком поставлены TRB.
        let slot = self.int_slot;
        self.queue_report_at(slot);
        self.int_slot = (self.int_slot + 1) % KBD_REPORTS;
    }
}

pub fn poll() {
    if let Some(x) = XHCI.lock_irq().as_mut() {
        if x.int_ring != 0 {
            unsafe { x.poll_hid() }
        }
    }
}

/// Поднята ли USB-клавиатура. Нужно `irq_mask_stdin`: у USB нет прерывания (опрос), поэтому в
/// режиме сна-до-ввода таймер держим ВКЛ — иначе на её нажатия ничего не проснётся.
pub fn has_keyboard() -> bool {
    XHCI.lock_irq().as_ref().map_or(false, |x| x.int_ring != 0)
}

// ─── накопитель наружу (Веха 196) ─────────────────────────────────────────────────────────────
//
// Интерфейс тот же, что у [`crate::ahci`] и [`crate::nvme`], и это не подражание ради красоты:
// выше слоя носителя разницы между диском и флешкой быть не должно, иначе каждая программа
// начнёт знать, откуда живёт система.

/// Тот же тип MBR-раздела, что ищут AHCI и NVMe: след явного согласия человека отдать носитель
/// под VOID. Флешку без него не трогаем — на ней чужие файлы.
const VOID_STORE_TYPE: u8 = crate::ahci::VOID_STORE_TYPE;

/// Есть ли накопитель и сколько у него блоков ВСЕГО (0 — накопителя нет). Для установщика.
pub fn disk_sectors() -> u64 {
    XHCI.lock_irq().as_ref().and_then(|x| x.msc).map_or(0, |m| m.blocks)
}

/// Искать на флешке раздел VOID и сделать её носителем store. `false` — флешки нет либо
/// раздела VOID на ней нет (тогда её не трогаем вовсе).
pub fn store_init() -> bool {
    let mut sec = [0u8; SECTOR];
    if !read_abs(0, &mut sec) {
        return false;
    }
    if sec[510] != 0x55 || sec[511] != 0xaa {
        return false;
    }
    for i in 0..4 {
        let e = &sec[446 + i * 16..446 + (i + 1) * 16];
        if e[4] != VOID_STORE_TYPE {
            continue;
        }
        let base = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
        let cap = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
        if base == 0 || cap == 0 {
            continue;
        }
        let mut g = XHCI.lock_irq();
        if let Some(m) = g.as_mut().and_then(|x| x.msc.as_mut()) {
            m.base = base;
            m.capacity = cap;
            return true;
        }
    }
    false
}

/// Ёмкость раздела store на флешке, секторов.
pub fn capacity_sectors() -> u64 {
    XHCI.lock_irq().as_ref().and_then(|x| x.msc).map_or(0, |m| m.capacity)
}

/// Чтение сектора store (со смещением раздела).
pub fn read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let base = XHCI.lock_irq().as_ref().and_then(|x| x.msc).map_or(0, |m| m.base);
    base != 0 && read_abs(base + sector, buf)
}

/// Запись сектора store (со смещением раздела).
pub fn write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let base = XHCI.lock_irq().as_ref().and_then(|x| x.msc).map_or(0, |m| m.base);
    base != 0 && write_abs(base + sector, buf)
}

/// Абсолютное чтение — им пользуются поиск раздела и установщик.
fn read_abs(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    let mut g = XHCI.lock_irq();
    let Some(x) = g.as_mut() else { return false };
    unsafe { x.rw_sector(false, sector, buf) }
}

fn write_abs(sector: u64, buf: &[u8; SECTOR]) -> bool {
    let mut g = XHCI.lock_irq();
    let Some(x) = g.as_mut() else { return false };
    let mut tmp = *buf;
    unsafe { x.rw_sector(true, sector, &mut tmp) }
}

// ─── установщик (Веха 196) ────────────────────────────────────────────────────────────────────

/// Номер, под которым USB-накопители называются установщику. Третья сотня: порты AHCI занимают
/// первые номера, NVMe — сотню (см. [`crate::nvme::SLOT_BASE`]), флешки — двести. Смешивать
/// нельзя по той же причине: номер обязан значить одно и то же до и после перезагрузки.
pub const SLOT_BASE: usize = 200;

/// Перечислить USB-накопители для установщика. Возвращает, сколько записано в `out`.
pub fn disks(out: &mut [crate::ahci::Disk]) -> usize {
    if out.is_empty() {
        return 0;
    }
    let (blocks, base) = {
        let g = XHCI.lock_irq();
        match g.as_ref().and_then(|x| x.msc) {
            Some(m) => (m.blocks, m.base),
            None => return 0,
        }
    };
    if blocks == 0 {
        return 0;
    }
    let mut model = [b' '; crate::ahci::MODEL_LEN];
    let name = b"USB";
    model[..name.len()].copy_from_slice(name);
    out[0] = crate::ahci::Disk {
        slot: SLOT_BASE,
        sectors: blocks,
        model,
        void: base != 0,
        // «С этой флешки работает система» — ровно тогда, когда store живёт на ней.
        live: crate::object::on_usb(),
    };
    1
}

/// Открыть флешку как ЦЕЛЬ установки. `false` — накопителя нет либо с него работает система.
pub fn target_open(slot: usize) -> bool {
    slot == SLOT_BASE && disk_sectors() != 0 && !crate::object::on_usb()
}

/// Полная ёмкость цели в секторах.
pub fn target_sectors() -> u64 {
    disk_sectors()
}

pub fn target_read(sector: u64, buf: &mut [u8; SECTOR]) -> bool {
    read_abs(sector, buf)
}

pub fn target_write(sector: u64, buf: &[u8; SECTOR]) -> bool {
    write_abs(sector, buf)
}
