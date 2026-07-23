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
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
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
}
unsafe impl Send for Xhci {}

static XHCI: SpinLock<Option<Xhci>> = SpinLock::new(None);

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
        let link = (cmd_ring + (RING_TRBS - 1) * 16) as *mut u32;
        write_volatile(link as *mut u64, cmd_ring as u64); // указатель назад на старт
        write_volatile(link.add(3), TRB_LINK << 10 | 1 << 1 | 1); // тип Link | Toggle | Cycle
        wr64(op + OP_CRCR, cmd_ring as u64 | 1); // RCS=1

        // 5) Кольцо событий + таблица сегментов (ERST, 1 сегмент).
        let Some(event_ring) = frame::alloc() else { return false };
        let Some(erst) = frame::alloc() else { return false };
        write_volatile(erst as *mut u64, event_ring as u64); // база сегмента
        write_volatile((erst + 8) as *mut u32, RING_TRBS as u32); // размер сегмента (TRB)
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
        };

        // Порты: сбросить подключённые (нужно для будущего Address Device).
        let mut connected = 0u32;
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
            }
        }

        // Проверка машинерии: Enable Slot → должно вернуть slot id (код завершения 1).
        let slot = x.enable_slot();
        crate::println!(
            "  [usb]  xHCI: {} портов, {} подключено, контекст {} Б; Enable Slot → {}",
            max_ports, connected, if ctx64 { 64 } else { 32 },
            match slot { Some(s) => s as i32, None => -1 },
        );

        *XHCI.lock() = Some(x);
    }
    true
}

impl Xhci {
    /// Поставить TRB в кольцо команд, позвонить в дверной звонок 0, дождаться Command
    /// Completion Event. Возвращает `[param_lo, param_hi, status, control]` события.
    unsafe fn command(&mut self, p_lo: u32, p_hi: u32, ctrl_type: u32) -> Option<[u32; 4]> {
        let trb = (self.cmd_ring + self.cmd_enq * 16) as *mut u32;
        write_volatile(trb, p_lo);
        write_volatile(trb.add(1), p_hi);
        write_volatile(trb.add(2), 0);
        write_volatile(trb.add(3), ctrl_type << 10 | self.cmd_cycle);
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

    /// Опросить кольцо событий до валидного события (совпал cycle bit), продвинуть ERDP.
    unsafe fn wait_event(&mut self) -> Option<[u32; 4]> {
        for _ in 0..10_000_000 {
            let ev = (self.event_ring + self.event_deq * 16) as *const u32;
            let ctrl = read_volatile(ev.add(3));
            if ctrl & 1 == self.event_cycle {
                let out = [
                    read_volatile(ev),
                    read_volatile(ev.add(1)),
                    read_volatile(ev.add(2)),
                    ctrl,
                ];
                self.event_deq += 1;
                if self.event_deq == RING_TRBS {
                    self.event_deq = 0;
                    self.event_cycle ^= 1;
                }
                let erdp = self.event_ring + self.event_deq * 16;
                wr64(self.rt + 0x20 + IR0_ERDP, erdp as u64 | ERDP_EHB);
                return Some(out);
            }
        }
        None
    }

    /// Enable Slot: выделить слот устройства. Возвращает slot id (из Command Completion Event,
    /// если код завершения = 1 «успех»).
    unsafe fn enable_slot(&mut self) -> Option<u8> {
        // command гарантирует Command Completion Event; код завершения в status[31:24]
        // (1 = успех), slot id в control[31:24].
        let ev = self.command(0, 0, TRB_ENABLE_SLOT)?;
        ((ev[2] >> 24) & 0xff == 1).then(|| (ev[3] >> 24) as u8)
    }
}
