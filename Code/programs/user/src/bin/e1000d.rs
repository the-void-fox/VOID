//! e1000d — драйвер Intel e1000 в USERSPACE (Веха 51, демо фундамента userspace-драйверов).
//!
//! Доказывает, что драйвер железа может жить в обычном процессе: карту он трогает НЕ через
//! syscall'ы ядра, а НАПРЯМУЮ — регистры замаплены в его адресный простор (`SYS_MMIO_MAP` по
//! MMIO-cap), кольца/буферы — DMA-память с известным физ-адресом (`SYS_DMA_ALLOC` по DMA-cap).
//! Это фундамент под хостинг Linux-драйверов: тот же доступ к MMIO/DMA, только сверху Linux-шим.
//!
//! Демо: замапить регистры e1000, сбросить карту, прочитать MAC (MMIO-чтение), настроить TX,
//! отправить один broadcast-кадр и дождаться, пока КАРТА выставит бит DD (Descriptor Done) —
//! значит устройство само прочитало наш DMA-дескриптор и буфер. MMIO(rw)+DMA доказаны из userspace.
#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};

use void_user as sys;

// Окна в нашем адресном пространстве (USER-регион, ниже кучи 0x6000_0000): регистры и DMA.
const MMIO_VA: usize = 0x5000_0000;
const TX_RING_VA: usize = 0x5100_0000;
const TX_BUF_VA: usize = 0x5101_0000;

// Регистры e1000 (смещения в BAR0).
const CTRL: usize = 0x0000;
const IMC: usize = 0x00d8;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const RAL: usize = 0x5400;
const RAH: usize = 0x5404;

const CTRL_RST: u32 = 1 << 26;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_ASDE: u32 = 1 << 5;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const DESC_DD: u8 = 1 << 0;
const TX_EOP: u8 = 1 << 0;
const TX_IFCS: u8 = 1 << 1;
const TX_RS: u8 = 1 << 3;

unsafe fn rd(off: usize) -> u32 {
    read_volatile((MMIO_VA + off) as *const u32)
}
unsafe fn wr(off: usize, v: u32) {
    write_volatile((MMIO_VA + off) as *mut u32, v);
}

fn write_mac(mac: &[u8; 6]) {
    let hex = b"0123456789abcdef";
    let mut out = [0u8; 17];
    for i in 0..6 {
        out[i * 3] = hex[(mac[i] >> 4) as usize];
        out[i * 3 + 1] = hex[(mac[i] & 0xf) as usize];
        if i < 5 {
            out[i * 3 + 2] = b':';
        }
    }
    sys::write(&out);
}

#[no_mangle]
pub extern "C" fn _start(mmio_cap: usize, dma_cap: usize) -> ! {
    // 1) Замапить регистры e1000 в свой адресный простор.
    if !sys::mmio_map(mmio_cap, MMIO_VA) {
        sys::write("[e1000d] не удалось замапить MMIO (нет cap?)\n".as_bytes());
        sys::exit(1);
    }
    // 2) Выделить DMA-страницы под TX-кольцо и буфер (физ-адреса — для железа).
    let (Some(ring_pa), Some(buf_pa)) =
        (sys::dma_alloc(dma_cap, TX_RING_VA), sys::dma_alloc(dma_cap, TX_BUF_VA))
    else {
        sys::write("[e1000d] не удалось выделить DMA (нет cap?)\n".as_bytes());
        sys::exit(1);
    };

    unsafe {
        // 3) Сброс карты, маскировка прерываний (опрос), линк вверх.
        wr(IMC, 0xffff_ffff);
        wr(CTRL, rd(CTRL) | CTRL_RST);
        for _ in 0..1_000_000 {
            if rd(CTRL) & CTRL_RST == 0 {
                break;
            }
        }
        wr(IMC, 0xffff_ffff);
        wr(CTRL, rd(CTRL) | CTRL_SLU | CTRL_ASDE);

        // 4) Прочитать MAC из фильтра RAL/RAH (MMIO-чтение из userspace).
        let (ral, rah) = (rd(RAL), rd(RAH));
        let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8,
                   rah as u8, (rah >> 8) as u8];
        sys::write("[e1000d] userspace-драйвер поднят, MAC ".as_bytes());
        write_mac(&mac);
        sys::write(b"\n");

        // 5) TX-кольцо (8 дескрипторов = 128 Б, как требует TDLEN); используем дескриптор 0.
        wr(TDBAL, ring_pa as u32);
        wr(TDBAH, (ring_pa as u64 >> 32) as u32);
        wr(TDLEN, 128);
        wr(TDH, 0);
        wr(TDT, 0);
        wr(TCTL, TCTL_EN | TCTL_PSP | 0x0f << 4 | 0x40 << 12);
        wr(TIPG, 0x0060_200a);

        // 6) Собрать 60-байтный broadcast-кадр в DMA-буфере (dest ff.., src=наш MAC, ethertype 0x88b5).
        let buf = TX_BUF_VA as *mut u8;
        for i in 0..6 {
            write_volatile(buf.add(i), 0xff); // dest broadcast
            write_volatile(buf.add(6 + i), mac[i]); // src
        }
        write_volatile(buf.add(12), 0x88); // ethertype 0x88B5 (локальный эксперимент)
        write_volatile(buf.add(13), 0xb5);
        for i in 14..60 {
            write_volatile(buf.add(i), b'V'); // полезная нагрузка
        }

        // 7) Дескриптор 0: адрес буфера, длина, cmd=EOP|IFCS|RS. Двинуть TDT — отдать карте.
        let d = TX_RING_VA as *mut u8;
        write_volatile(d as *mut u64, buf_pa as u64);
        write_volatile(d.add(8) as *mut u16, 60);
        write_volatile(d.add(11), TX_EOP | TX_IFCS | TX_RS);
        write_volatile(d.add(12), 0u8); // status (карта выставит DD)
        wr(TDT, 1);

        // 8) Опрос: карта должна выставить DD — значит она сама прочитала DMA-дескриптор и буфер.
        let mut done = false;
        for _ in 0..10_000_000 {
            if read_volatile(d.add(12)) & DESC_DD != 0 {
                done = true;
                break;
            }
        }
        if done {
            sys::write("[e1000d] TX: карта выставила DD -- MMIO+DMA из userspace РАБОТАЮТ\n".as_bytes());
        } else {
            sys::write("[e1000d] TX: DD не выставлен (таймаут)\n".as_bytes());
        }
    }
    sys::exit(0);
}
