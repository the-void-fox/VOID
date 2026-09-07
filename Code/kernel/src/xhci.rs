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
pub struct Xhci {
    op: usize, // база операционных регистров
    db: usize, // база массива дверных звонков
    rt: usize, // база runtime-регистров
    max_ports: u32,
    ctx64: bool, // размер контекста 64 Б (иначе 32)
    cmd_ring: usize, // кольцо команд (RING_TRBS × 16 Б, последний — Link)
    cmd_enq: usize, // индекс постановки команды
    cmd_cycle: u32, // producer cycle state кольца команд
    event_ring: usize,
    event_deq: usize,
    event_cycle: u32, // consumer cycle state кольца событий
    dcbaa: usize, // массив базовых адресов контекстов устройств
    // Часть B — перечисленное устройство (одно; для клавиатуры хватает):
    slot: u8, // slot id (0 — нет устройства)
    ep0_ring: usize, // TR-кольцо управляющего эндпоинта EP0
    ep0_enq: usize,
    ep0_cycle: u32,
    dma_buf: usize, // буфер под дескрипторы (один фрейм, DMA)
    // Часть C — HID-клавиатура (interrupt IN эндпоинт):
    int_ring: usize, // TR-кольцо interrupt-эндпоинта (0 — не настроен)
    int_enq: usize,
    int_cycle: u32,
    int_dci: u32, // Device Context Index interrupt-эндпоинта (звонок)
    int_buf: usize, // буфер под 8-байтные boot-репорты
    prev: [u8; 6], // предыдущий набор нажатых клавиш (для детекта НОВЫХ нажатий)
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
    let Some(base) = crate::arch::probe_xhci() else {
        return false;
    };
    unsafe {
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
        let Some(dcbaa) = frame::alloc() else { return false };
        wr64(op + OP_DCBAAP, dcbaa as u64);

        // 4) Кольцо команд: Link TRB в конце заворачивает на начало (Toggle Cycle).
        let Some(cmd_ring) = frame::alloc() else { return false };
        let link = dm(cmd_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, cmd_ring as u64); // указатель назад на старт
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1); // тип Link | Toggle | Cycle
        wr64(op + OP_CRCR, cmd_ring as u64 | 1); // RCS=1

        // 5) Кольцо событий + таблица сегментов (ERST, 1 сегмент).
        let Some(event_ring) = frame::alloc() else { return false };
        let Some(erst) = frame::alloc() else { return false };
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
            op, db, rt, max_ports, ctx64,
            cmd_ring, cmd_enq: 0, cmd_cycle: 1,
            event_ring, event_deq: 0, event_cycle: 1,
            dcbaa,
            slot: 0, ep0_ring: 0, ep0_enq: 0, ep0_cycle: 1, dma_buf: 0,
            int_ring: 0, int_enq: 0, int_cycle: 1, int_dci: 0, int_buf: 0, prev: [0; 6],
        };

        // Порты: сбросить подключённые, запомнить ПЕРВЫЙ с устройством (порт + скорость).
        let mut connected = 0u32;
        let mut dev_port = 0u32;
        let mut dev_speed = 0u32;
        for p in 1..=max_ports {
            let psc = op + OP_PORTS + (p as usize - 1) * 0x10;
            let v = rd(psc);
            if v & PORTSC_CCS != 0 {
                connected += 1;
                // Сброс порта: PR=1, сохранив CCS/PP, не трогая RW1C-изменения.
                wr(psc, v & !PORTSC_CHANGES | PORTSC_PR);
                for _ in 0..1_000_000 {
                    if rd(psc) & PORTSC_PED != 0 {
                        break;
                    }
                }
                wr(psc, rd(psc) & !PORTSC_CHANGES | PORTSC_CHANGES); // сбросить биты-изменения
                if dev_port == 0 && rd(psc) & PORTSC_PED != 0 {
                    dev_port = p;
                    dev_speed = (rd(psc) >> 10) & 0xf; // Port Speed [13:10]
                }
            }
        }

        // Часть B/C — перечислить устройство и, если это HID-клавиатура, настроить её.
        let mut desc = [0u8; 18];
        let mut hid = false;
        let mut enumerated = false;
        if dev_port != 0 {
            if let Some(slot) = x.enable_slot() {
                x.slot = slot;
                if x.address_device(slot, dev_port, dev_speed)
                    && x.get_descriptor(1, 0, &mut desc)
                {
                    enumerated = true;
                    hid = x.setup_hid(dev_speed);
                }
            }
        }
        if hid {
            crate::println!(
                "  [usb]  xHCI: HID-клавиатура на порту {} (slot {}) — ввод по USB готов",
                dev_port, x.slot,
            );
        } else if enumerated {
            let vid = desc[8] as u16 | (desc[9] as u16) << 8;
            let pid = desc[10] as u16 | (desc[11] as u16) << 8;
            crate::println!(
                "  [usb]  xHCI: устройство {:04x}:{:04x} class={} (не HID-клавиатура — ввод не настроен)",
                vid, pid, desc[4],
            );
        } else {
            crate::println!(
                "  [usb]  xHCI: {} портов, {} подключено — перечисление не удалось",
                max_ports, connected,
            );
        }

        *XHCI.lock_irq() = Some(x);
    }
    true
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
    /// устройства в DCBAA[slot] и TR-кольцо EP0, выдать команду. `true` — устройство получило
    /// адрес и EP0 готов к control-трансферам.
    unsafe fn address_device(&mut self, slot: u8, port: u32, speed: u32) -> bool {
        let cs = if self.ctx64 { 64 } else { 32 };
        let (Some(dev_ctx), Some(ep0_ring), Some(input), Some(dma)) =
            (frame::alloc(), frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return false;
        };
        write_volatile(dm(self.dcbaa + slot as usize * 8) as *mut u64, dev_ctx as u64);
        self.dma_buf = dma;
        // TR-кольцо EP0 с Link-заворотом.
        let link = dm(ep0_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, ep0_ring as u64);
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1);
        self.ep0_ring = ep0_ring;
        self.ep0_enq = 0;
        self.ep0_cycle = 1;
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
        matches!(ev, Some(e) if (e[2] >> 24) & 0xff == 1)
    }

    /// Поставить TRB в TR-кольцо EP0 (с заворотом на Link).
    unsafe fn push_ep0(&mut self, p_lo: u32, p_hi: u32, status: u32, control: u32) {
        let trb = dm(self.ep0_ring + self.ep0_enq * 16) as *mut u32;
        write_volatile(trb, p_lo);
        write_volatile(trb.add(1), p_hi);
        write_volatile(trb.add(2), status);
        write_volatile(trb.add(3), control | self.ep0_cycle);
        self.ep0_enq += 1;
        if self.ep0_enq == RING_TRBS - 1 {
            self.ep0_enq = 0;
            self.ep0_cycle ^= 1;
        }
    }

    /// Управляющий IN-трансфер по EP0: Setup + Data(IN) + Status(OUT, IOC), звонок EP0, ждём
    /// Transfer Event. Данные приходят в [`Self::dma_buf`]. `true` — успех/короткий пакет.
    unsafe fn control_in(&mut self, req_type: u8, request: u8, value: u16, index: u16, len: u16) -> bool {
        let setup = req_type as u64 | (request as u64) << 8 | (value as u64) << 16
            | (index as u64) << 32 | (len as u64) << 48;
        // Setup Stage(2): IDT(бит6), TRT=IN(3) в [17:16].
        self.push_ep0(setup as u32, (setup >> 32) as u32, 8, TRB_SETUP << 10 | 3 << 16 | 1 << 6);
        // Data Stage(3): DIR=IN(бит16).
        self.push_ep0(self.dma_buf as u32, (self.dma_buf as u64 >> 32) as u32, len as u32,
            TRB_DATA << 10 | 1 << 16);
        // Status Stage(4): DIR=OUT, IOC(бит5).
        self.push_ep0(0, 0, 0, TRB_STATUS << 10 | 1 << 5);
        compiler_fence(Ordering::SeqCst);
        wr(self.db + self.slot as usize * 4, 1); // звонок EP0 (DCI 1)
        for _ in 0..64 {
            let Some(ev) = self.wait_event() else { return false };
            if (ev[3] >> 10) & 0x3f == EV_TRANSFER {
                let code = (ev[2] >> 24) & 0xff;
                return code == 1 || code == 13; // успех или короткий пакет
            }
        }
        false
    }

    /// GET_DESCRIPTOR по EP0 → скопировать `out.len()` байт из DMA-буфера.
    unsafe fn get_descriptor(&mut self, dtype: u8, index: u8, out: &mut [u8]) -> bool {
        let value = (dtype as u16) << 8 | index as u16;
        if !self.control_in(0x80, 6, value, 0, out.len() as u16) {
            return false;
        }
        core::ptr::copy_nonoverlapping(dm(self.dma_buf) as *const u8, out.as_mut_ptr(), out.len());
        true
    }

    /// Управляющий трансфер БЕЗ данных (SET_CONFIGURATION, SET_PROTOCOL): Setup + Status(IN,IOC).
    unsafe fn control_nodata(&mut self, req_type: u8, request: u8, value: u16, index: u16) -> bool {
        let setup = req_type as u64 | (request as u64) << 8 | (value as u64) << 16
            | (index as u64) << 32; // wLength=0
        self.push_ep0(setup as u32, (setup >> 32) as u32, 8, TRB_SETUP << 10 | 1 << 6); // TRT=No Data
        self.push_ep0(0, 0, 0, TRB_STATUS << 10 | 1 << 16 | 1 << 5); // Status DIR=IN, IOC
        compiler_fence(Ordering::SeqCst);
        wr(self.db + self.slot as usize * 4, 1);
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
    unsafe fn setup_hid(&mut self, speed: u32) -> bool {
        let mut cfg = [0u8; 96];
        if !self.get_descriptor(2, 0, &mut cfg) {
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
        self.control_nodata(0x00, 9, 1, 0);
        self.control_nodata(0x21, 0x0b, 0, iface as u16);
        self.configure_endpoint(ep_addr, ep_mps, ep_ivl, speed) && {
            self.queue_report();
            true
        }
    }

    /// Configure Endpoint: добавить interrupt-IN эндпоинт в контекст устройства.
    unsafe fn configure_endpoint(&mut self, ep_addr: u8, mps: u16, ivl: u8, speed: u32) -> bool {
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
            TRB_CONFIG_EP << 10 | (self.slot as u32) << 24);
        matches!(ev, Some(e) if (e[2] >> 24) & 0xff == 1)
    }

    /// Поставить Normal-TRB на interrupt-кольцо (приём одного 8-байтного репорта) + звонок.
    unsafe fn queue_report(&mut self) {
        let trb = dm(self.int_ring + self.int_enq * 16) as *mut u32;
        write_volatile(trb as *mut u64, self.int_buf as u64);
        write_volatile(trb.add(2), 8); // длина буфера
        write_volatile(trb.add(3), TRB_NORMAL << 10 | 1 << 5 | 1 << 2 | self.int_cycle); // IOC|ISP
        self.int_enq += 1;
        if self.int_enq == RING_TRBS - 1 {
            self.int_enq = 0;
            self.int_cycle ^= 1;
        }
        compiler_fence(Ordering::SeqCst);
        wr(self.db + self.slot as usize * 4, self.int_dci); // звонок interrupt-эндпоинта
    }

    /// Опрос: разобрать пришедшие boot-репорты клавиатуры, отдать НОВЫЕ нажатия в консоль.
    unsafe fn poll_hid(&mut self) {
        while let Some(ev) = self.try_event() {
            if (ev[3] >> 10) & 0x3f != EV_TRANSFER {
                continue; // не трансфер (порт/команда) — пропустить
            }
            let r = core::slice::from_raw_parts(self.int_buf as *const u8, 8);
            let mods = r[0];
            let shift = mods & 0x22 != 0; // Left/Right Shift
            for &k in &r[2..8] {
                // Новое нажатие: код есть в этом репорте, но не было в прошлом.
                if k != 0 && !self.prev.contains(&k) {
                    if let Some(b) = hid_to_ascii(k, shift) {
                        crate::arch::usb_key(b);
                    }
                }
            }
            self.prev.copy_from_slice(&r[2..8]);
            self.queue_report(); // подставить буфер под следующий репорт
        }
    }
}

/// HID Usage (boot keyboard) → ASCII. Достаточно для vsh: буквы, цифры, пробел, Enter, Backspace,
/// Tab, базовая пунктуация; Shift даёт верхний регистр/символы. Неизвестное — `None`.
fn hid_to_ascii(k: u8, shift: bool) -> Option<u8> {
    let b = match k {
        0x04..=0x1d => {
            let c = b'a' + (k - 0x04);
            return Some(if shift { c - 32 } else { c });
        }
        0x1e..=0x26 => {
            let d = b'1' + (k - 0x1e);
            let sym = [b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'('];
            return Some(if shift { sym[(k - 0x1e) as usize] } else { d });
        }
        0x27 => if shift { b')' } else { b'0' },
        0x28 => b'\r', // Enter
        0x2a => 0x08,  // Backspace
        0x2b => b'\t', // Tab
        0x2c => b' ',  // Space
        0x2d => if shift { b'_' } else { b'-' },
        0x2e => if shift { b'+' } else { b'=' },
        0x2f => if shift { b'{' } else { b'[' },
        0x30 => if shift { b'}' } else { b']' },
        0x31 => if shift { b'|' } else { b'\\' },
        0x33 => if shift { b':' } else { b';' },
        0x34 => if shift { b'"' } else { b'\'' },
        0x36 => if shift { b'<' } else { b',' },
        0x37 => if shift { b'>' } else { b'.' },
        0x38 => if shift { b'?' } else { b'/' },
        _ => return None,
    };
    Some(b)
}

/// Опрос USB-клавиатуры — зовётся из `console_drain` (тик/IRQ) наравне с PS/2 и COM1.
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
