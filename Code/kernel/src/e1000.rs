//! Драйвер Intel e1000 (PRO/1000) — сетевая карта на РЕАЛЬНОМ x86-железе (Веха 49).
//!
//! Даёт userspace-стеку (`net-srv`: ARP/IPv4/ICMP, Веха 34) тот же интерфейс сырых Ethernet-
//! кадров, что и [`crate::virtio_net`], но поверх регистров e1000. Карту ищет арх
//! ([`crate::arch::probe_e1000`] — PCI 8086:100e/10d3, BAR0), разговор — здесь. На riscv/QEMU-virt
//! e1000 нет — `probe` вернёт `None`, драйвер откатится на virtio-net.
//!
//! Ввод-вывод **опросом** (без прерываний) — как virtio-net: net-srv опрашивает кольца. Кольца
//! дескрипторов и буферы — в обнулённых фреймах [`crate::frame`] (RAM отображена идентично,
//! физ. адрес = вирт., карта читает/пишет их DMA; на x86 DMA когерентен).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};

use crate::frame;
use crate::sync::SpinLock;

// ─── регистры (смещения в BAR0) ──────────────────────────────────────────────
const CTRL: usize = 0x0000; // управление устройством
const EERD: usize = 0x0014; // чтение EEPROM
const IMC: usize = 0x00d8; // маска прерываний — сброс (мы опрашиваем)
const RCTL: usize = 0x0100; // управление приёмом
const TCTL: usize = 0x0400; // управление передачей
const TIPG: usize = 0x0410; // межпакетный интервал передачи
const RDBAL: usize = 0x2800; // RX-кольцо: адрес (низ/верх), длина, голова/хвост
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800; // TX-кольцо
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const RAL: usize = 0x5400; // фильтр приёма = наш MAC (прошивка/QEMU уже прописала)
const RAH: usize = 0x5404;
const MTA: usize = 0x5200; // таблица multicast (128 слов) — обнуляем

const CTRL_RST: u32 = 1 << 26; // сброс
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_ASDE: u32 = 1 << 5; // auto-speed detect

const RCTL_EN: u32 = 1 << 1; // приём вкл
const RCTL_BAM: u32 = 1 << 15; // принимать broadcast
const RCTL_SECRC: u32 = 1 << 26; // срезать CRC (буфер = чистый кадр)

const TCTL_EN: u32 = 1 << 1; // передача вкл
const TCTL_PSP: u32 = 1 << 3; // дополнять короткие пакеты

const DESC_DD: u8 = 1 << 0; // Descriptor Done — карта отработала дескриптор
const TX_EOP: u8 = 1 << 0; // End Of Packet
const TX_IFCS: u8 = 1 << 1; // вставить контрольную сумму кадра
const TX_RS: u8 = 1 << 3; // сообщить статус (выставить DD по завершении)

const NRX: usize = 16; // дескрипторов приёма (по одному буферу-фрейму на каждый)
const NTX: usize = 8; // дескрипторов передачи
const BUF: usize = 2048; // размер буфера кадра (BSIZE по умолчанию у RCTL)

struct E1000 {
    base: usize,
    mac: [u8; 6],
    rx_ring: usize, // NRX × 16 байт (дескриптор приёма)
    tx_ring: usize, // NTX × 16 байт (дескриптор передачи)
    rx_cur: usize, // следующий дескриптор приёма для проверки
    tx_cur: usize, // следующий дескриптор передачи для использования
}
unsafe impl Send for E1000 {}

static E1000: SpinLock<Option<E1000>> = SpinLock::new(None);

#[inline]
unsafe fn rd(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}
#[inline]
unsafe fn wr(base: usize, off: usize, v: u32) {
    write_volatile((base + off) as *mut u32, v);
}

// Поля дескриптора приёма (16 байт): addr@0(u64), len@8(u16), csum@10, status@12(u8).
// Поля дескриптора передачи (16 байт): addr@0(u64), len@8(u16), cso@10, cmd@11, status@12.
#[inline]
unsafe fn desc_addr(ring: usize, i: usize) -> usize {
    read_volatile(crate::frame::ptr(ring + i * 16) as *const u64) as usize
}

/// Прочитать MAC карты: сперва из фильтра RAL/RAH (прошивка/QEMU его прописывают), иначе из EEPROM.
fn read_mac(base: usize) -> [u8; 6] {
    unsafe {
        let (ral, rah) = (rd(base, RAL), rd(base, RAH));
        if ral != 0 || rah & 0xffff != 0 {
            return [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8,
                    rah as u8, (rah >> 8) as u8];
        }
        // EEPROM: адрес в биты [15:8], START=1; готово — бит DONE(4), слово — в [31:16].
        let mut mac = [0u8; 6];
        for i in 0..3usize {
            wr(base, EERD, (i as u32) << 8 | 1);
            let mut w = 0u32;
            for _ in 0..100_000 {
                let e = rd(base, EERD);
                if e & (1 << 4) != 0 {
                    w = e >> 16;
                    break;
                }
            }
            mac[i * 2] = w as u8;
            mac[i * 2 + 1] = (w >> 8) as u8;
        }
        mac
    }
}

/// Инициализировать e1000, если он есть. `true` — карта готова к приёму/передаче.
pub fn init() -> bool {
    let Some(base) = crate::arch::probe_e1000() else {
        return false;
    };
    let Some(ring_frame) = frame::alloc() else { return false };
    let rx_ring = ring_frame; // RX-кольцо в начале фрейма
    let tx_ring = ring_frame + 0x200; // TX-кольцо со смещением (оба << 4 КиБ)

    unsafe {
        // Сброс карты, маскировка прерываний (опрашиваем), поднять линк.
        wr(base, IMC, 0xffff_ffff);
        wr(base, CTRL, rd(base, CTRL) | CTRL_RST);
        for _ in 0..1_000_000 {
            if rd(base, CTRL) & CTRL_RST == 0 {
                break;
            }
        }
        wr(base, IMC, 0xffff_ffff);
        wr(base, CTRL, rd(base, CTRL) | CTRL_SLU | CTRL_ASDE);
        for i in 0..128 {
            wr(base, MTA + i * 4, 0); // очистить multicast-фильтр
        }

        // RX: каждому дескриптору — свой буфер-фрейм; кольцо, длина, голова=0, хвост=NRX-1.
        for i in 0..NRX {
            let Some(buf) = frame::alloc() else { return false };
            let d = crate::frame::ptr(rx_ring + i * 16);
            write_volatile(d as *mut u64, buf as u64);
            write_volatile(d.add(12), 0u8); // status=0 (карта заполнит)
        }
        wr(base, RDBAL, rx_ring as u32);
        wr(base, RDBAH, (rx_ring as u64 >> 32) as u32);
        wr(base, RDLEN, (NRX * 16) as u32);
        wr(base, RDH, 0);
        wr(base, RDT, (NRX - 1) as u32);
        wr(base, RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC); // BSIZE=2048 (по умолчанию)

        // TX: буферы, кольцо; status=DD (дескриптор свободен), голова=хвост=0.
        for i in 0..NTX {
            let Some(buf) = frame::alloc() else { return false };
            let d = crate::frame::ptr(tx_ring + i * 16);
            write_volatile(d as *mut u64, buf as u64);
            write_volatile(d.add(12), DESC_DD);
        }
        wr(base, TDBAL, tx_ring as u32);
        wr(base, TDBAH, (tx_ring as u64 >> 32) as u32);
        wr(base, TDLEN, (NTX * 16) as u32);
        wr(base, TDH, 0);
        wr(base, TDT, 0);
        // CT=15 (порог коллизий), COLD=0x40 (дистанция) — стандарт для полудуплекса; безвредно.
        wr(base, TCTL, TCTL_EN | TCTL_PSP | 0x0f << 4 | 0x40 << 12);
        wr(base, TIPG, 0x0060_200a); // рекомендованный IPG для медного PHY
    }

    let mac = read_mac(base);
    *E1000.lock() = Some(E1000 { base, mac, rx_ring, tx_ring, rx_cur: 0, tx_cur: 0 });
    true
}

impl E1000 {
    fn send(&mut self, frame_bytes: &[u8]) -> bool {
        if frame_bytes.is_empty() || frame_bytes.len() > BUF {
            return false;
        }
        unsafe {
            let i = self.tx_cur;
            let d = crate::frame::ptr(self.tx_ring + i * 16);
            let buf = desc_addr(self.tx_ring, i);
            // `buf` — физический адрес буфера кольца (его знает карта): пишем через direct-map.
            core::ptr::copy_nonoverlapping(
                frame_bytes.as_ptr(),
                crate::frame::ptr(buf),
                frame_bytes.len(),
            );
            write_volatile(d.add(8) as *mut u16, frame_bytes.len() as u16); // len
            write_volatile(d.add(11), TX_EOP | TX_IFCS | TX_RS); // cmd
            write_volatile(d.add(12), 0u8); // status (карта выставит DD)
            compiler_fence(Ordering::SeqCst);
            self.tx_cur = (i + 1) % NTX;
            wr(self.base, TDT, self.tx_cur as u32); // отдать карте
            for _ in 0..10_000_000 {
                if read_volatile(d.add(12)) & DESC_DD != 0 {
                    return true; // отправлено
                }
            }
            true // таймаут статуса — кадр всё равно поставлен в очередь
        }
    }

    fn recv(&mut self, out: &mut [u8]) -> usize {
        unsafe {
            let i = self.rx_cur;
            let d = (self.rx_ring + i * 16) as *mut u8;
            if read_volatile(d.add(12)) & DESC_DD == 0 {
                return 0; // приёмник пуст
            }
            let len = read_volatile(d.add(8) as *const u16) as usize;
            let buf = desc_addr(self.rx_ring, i);
            let n = len.min(out.len());
            core::ptr::copy_nonoverlapping(crate::frame::ptr(buf) as *const u8, out.as_mut_ptr(), n);
            write_volatile(d.add(12), 0u8); // вернуть дескриптор карте (status=0)
            compiler_fence(Ordering::SeqCst);
            wr(self.base, RDT, i as u32); // хвост = обработанный дескриптор
            self.rx_cur = (i + 1) % NRX;
            n
        }
    }
}

/// MAC-адрес карты (0…0, если не инициализирована).
pub fn mac() -> [u8; 6] {
    E1000.lock().as_ref().map_or([0u8; 6], |n| n.mac)
}

/// Отправить Ethernet-кадр. `false` — карты нет или кадр негоден.
pub fn send(frame_bytes: &[u8]) -> bool {
    match E1000.lock().as_mut() {
        Some(n) => n.send(frame_bytes),
        None => false,
    }
}

/// Принять один кадр (неблокирующе). Возвращает число байт (0 — приёмник пуст).
pub fn recv(out: &mut [u8]) -> usize {
    match E1000.lock().as_mut() {
        Some(n) => n.recv(out),
        None => 0,
    }
}
