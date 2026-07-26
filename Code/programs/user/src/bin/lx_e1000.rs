//! lx_e1000 — Intel e1000 как Linux-СТИЛЕВОЙ драйвер поверх шима [`void_user::lx_emul`] (Веха 53).
//!
//! Тот же e1000, что в `e1000d` (Вехи 51–52), но карту он трогает НЕ syscall'ами и не сырыми
//! примитивами ядра, а Linux-подобным API: `ioremap`/`readl`/`writel`, `dma_alloc_coherent`,
//! `request_irq` (threaded), `wait_for_completion`, входная точка `probe`. Ни строчки о capability
//! или syscall'ах — их прячет `lx_emul`. Доказывает driver-model и threaded-IRQ каркаса: следующий
//! шаг — заменить этот рукописный «Linux-драйвер» реальными `.c` из ядра Linux (Genode `dde_linux`).

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicUsize, Ordering};

use void_user as sys;
use void_user::lx_emul as lx;

// Регистры e1000 (смещения в BAR0) — как в e1000d.
const CTRL: usize = 0x0000;
const ICR: usize = 0x00c0;
const ICS: usize = 0x00c8;
const IMS: usize = 0x00d0;
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
const ICR_LSC: u32 = 1 << 2;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const DESC_DD: u8 = 1 << 0;
const TX_EOP: u8 = 1 << 0;
const TX_IFCS: u8 = 1 << 1;
const TX_RS: u8 = 1 << 3;

// База MMIO — заполняется в probe, читается нитью-обработчиком IRQ.
static MMIO: AtomicUsize = AtomicUsize::new(0);
// Сигнал «прерывание обработано» из нити-IRQ в probe.
static DONE: lx::Completion = lx::Completion::new();

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
    lx::printk(&out);
}

/// Threaded-обработчик прерывания (крутится в нити `request_irq`): прочитать ICR (снимает линию),
/// напечатать причину, снова замаскировать, сигналить `probe` через completion.
fn e1000_irq() {
    let mmio = MMIO.load(Ordering::SeqCst) as *mut u8;
    let cause = unsafe { lx::readl(mmio.add(ICR)) };
    lx::printk("[lx_e1000] IRQ! threaded-обработчик разбужен, ICR=".as_bytes());
    lx::printk_hex(cause);
    lx::printk(" -- lx_emul: request_irq РАБОТАЕТ\n".as_bytes());
    unsafe { lx::writel(0xffff_ffff, mmio.add(IMC)) }; // замаскировать причины — уходим
    DONE.complete();
}

/// `probe` — как у Linux-драйвера: поднять устройство из ресурсов `dev`. Всё железо — через шим.
fn probe(dev: &lx::Device) -> i32 {
    // 1) ioremap регистров.
    let mmio = lx::ioremap(dev.mmio_cap, 0x0002_0000);
    if mmio.is_null() {
        lx::printk("[lx_e1000] ioremap отказал (нет MMIO-cap?)\n".as_bytes());
        return 1;
    }
    MMIO.store(mmio as usize, Ordering::SeqCst);

    unsafe {
        // 2) Сброс карты, маскировка прерываний, линк вверх.
        lx::writel(0xffff_ffff, mmio.add(IMC));
        lx::writel(lx::readl(mmio.add(CTRL)) | CTRL_RST, mmio.add(CTRL));
        for _ in 0..1_000_000 {
            if lx::readl(mmio.add(CTRL)) & CTRL_RST == 0 {
                break;
            }
        }
        lx::writel(0xffff_ffff, mmio.add(IMC));
        lx::writel(lx::readl(mmio.add(CTRL)) | CTRL_SLU | CTRL_ASDE, mmio.add(CTRL));

        // 3) MAC из RAL/RAH.
        let (ral, rah) = (lx::readl(mmio.add(RAL)), lx::readl(mmio.add(RAH)));
        let mac = [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8,
                   rah as u8, (rah >> 8) as u8];
        lx::printk("[lx_e1000] драйвер поднят на lx_emul, MAC ".as_bytes());
        write_mac(&mac);
        lx::printk(b"\n");

        // 4) DMA: кольцо TX + буфер кадра.
        let ring = lx::dma_alloc_coherent(dev.dma_cap, 4096);
        let buf = lx::dma_alloc_coherent(dev.dma_cap, 4096);
        if ring.cpu.is_null() || buf.cpu.is_null() {
            lx::printk("[lx_e1000] dma_alloc_coherent отказал (нет DMA-cap?)\n".as_bytes());
            return 1;
        }

        // 5) TX-кольцо (8 дескрипторов = 128 Б); дескриптор 0.
        lx::writel(ring.dma as u32, mmio.add(TDBAL));
        lx::writel((ring.dma as u64 >> 32) as u32, mmio.add(TDBAH));
        lx::writel(128, mmio.add(TDLEN));
        lx::writel(0, mmio.add(TDH));
        lx::writel(0, mmio.add(TDT));
        lx::writel(TCTL_EN | TCTL_PSP | 0x0f << 4 | 0x40 << 12, mmio.add(TCTL));
        lx::writel(0x0060_200a, mmio.add(TIPG));

        // 6) 60-байтный broadcast-кадр в DMA-буфере.
        let b = buf.cpu;
        for i in 0..6 {
            lx::writeb(0xff, b.add(i));
            lx::writeb(mac[i], b.add(6 + i));
        }
        lx::writeb(0x88, b.add(12)); // ethertype 0x88B5
        lx::writeb(0xb5, b.add(13));
        for i in 14..60 {
            lx::writeb(b'V', b.add(i));
        }

        // 7) Дескриптор 0: адрес буфера, длина, cmd; двинуть TDT.
        let d = ring.cpu;
        lx::writeq(buf.dma as u64, d);
        lx::writew(60, d.add(8));
        lx::writeb(TX_EOP | TX_IFCS | TX_RS, d.add(11));
        lx::writeb(0, d.add(12));
        lx::writel(1, mmio.add(TDT));

        // 8) Опрос DD — карта прочитала DMA-дескриптор и буфер.
        let mut done = false;
        for _ in 0..10_000_000 {
            if lx::readb(d.add(12)) & DESC_DD != 0 {
                done = true;
                break;
            }
        }
        if done {
            lx::printk("[lx_e1000] TX: DD выставлен -- MMIO+DMA через lx_emul РАБОТАЮТ\n".as_bytes());
        } else {
            lx::printk("[lx_e1000] TX: DD не выставлен (таймаут)\n".as_bytes());
        }
    }

    // 9) Прерывание через request_irq (threaded) ВМЕСТО опроса.
    if dev.irq_cap == sys::NO_CAP {
        lx::printk("[lx_e1000] IRQ-cap не выдан -- прерывание пропущено (демо на опросе)\n".as_bytes());
        return 0;
    }
    if !lx::request_irq(dev.irq_cap, e1000_irq) {
        lx::printk("[lx_e1000] request_irq отказал\n".as_bytes());
        return 1;
    }
    unsafe {
        lx::writel(0xffff_ffff, mmio.add(ICR)); // сбросить залипшие причины
        let _ = lx::readl(mmio.add(ICR));
        lx::writel(ICR_LSC, mmio.add(IMS)); // размаскировать Link-Status-Change
        lx::writel(ICR_LSC, mmio.add(ICS)); // инициировать её — карта поднимет линию
    }
    lx::printk("[lx_e1000] request_irq + жду прерывание (wait_for_completion, опрос выключен)...\n".as_bytes());
    DONE.wait_for_completion();
    0
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    lx::module_init(probe)
}
