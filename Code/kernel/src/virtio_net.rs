//! Драйвер сетевой карты virtio-net (Веха 34).
//!
//! «Руки» для сетевого стека: умеет отправить и принять один Ethernet-кадр. Устройство
//! ищет АРХ ([`arch::probe_virtio_net`]): на QEMU `virt` (riscv) — слот virtio-mmio с
//! device id 1, на q35 (x86) — virtio-pci 0x1041. Транспорт тот же split virtqueue, что
//! у блока, но очередей ДВЕ: **0 = приём (RX)**, **1 = передача (TX)**; работа — ОПРОСОМ
//! колец (прерывания, как синхронный путь `virtio_blk` на загрузке, отложены — сетевому
//! серверу они не нужны: он и так активно ждёт ответа).
//!
//! Каждый кадр в virtio-net предваряется 12-байтным заголовком `virtio_net_hdr` (для нас
//! всегда нули: без контрольных сумм и GSO). Приёмные буферы засеваются в RX-кольцо при
//! инициализации; `recv` снимает завершённый буфер, отдаёт кадр (без заголовка) и
//! ПЕРЕвыставляет буфер. `send` — синхронный: один TX-буфер, публикуем и ждём used.
//!
//! Наверх драйвер отдаёт сырые кадры (`send`/`recv`/`mac`); ARP/IPv4/ICMP живут в
//! userspace-сервере `net-srv` (микроядерность: стек — не в ядре).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::virtio::{self, Cfg, Queue, DESC_F_WRITE};
use crate::arch;

/// ВМЕСТИМОСТЬ колец (верхняя граница числа дескрипторов). Сколько занято на самом деле —
/// `Queue::size()`: устройство объявляет свой максимум, и превысить его нельзя.
///
/// Веха 135.2 — было 8. Это потолок не только «сколько кадров влезет», но и СКОРОСТИ TCP: глубина
/// кольца — это то, сколько кадров переживёт приём, пока мы их не разгребли, а по этому числу
/// (`max_burst_size` в net_phy.rs) smoltcp зажимает объявляемое окно. С восьмёркой окно упиралось
/// в ~11.8 КиБ независимо от буферов.
const QSIZE: usize = 64;
/// Размер одного буфера: 12-байтный заголовок + кадр (Ethernet MTU 1514 + запас).
const BUF: usize = 2048;
/// Длина заголовка virtio_net_hdr (modern, VIRTIO_F_VERSION_1 → есть num_buffers).
const NET_HDR: usize = 12;

/// Feature-бит 5 (младший dword): `VIRTIO_NET_F_MAC` — устройство сообщает MAC в конфиг-области.
const FEATURE_LO_MAC: u32 = 1 << 5;

/// Инициализированная сетевая карта.
struct VirtioNet {
    rx: Queue,
    tx: Queue,
    /// Приёмные буферы (живут вечно, переиспользуются); индекс = id дескриптора.
    rx_bufs: Vec<Box<[u8; BUF]>>,
    /// Единственный буфер передачи (send синхронный — перекрытия нет).
    tx_buf: Box<[u8; BUF]>,
    mac: [u8; 6],
}

impl VirtioNet {
    /// Отправить Ethernet-кадр: 12-байтный нулевой заголовок + кадр, один дескриптор,
    /// ждём завершения в used TX-очереди. `false` — кадр не влез или устройства нет.
    fn send(&mut self, frame_bytes: &[u8]) -> bool {
        if frame_bytes.len() > BUF - NET_HDR {
            return false;
        }
        self.tx_buf[..NET_HDR].fill(0);
        self.tx_buf[NET_HDR..NET_HDR + frame_bytes.len()].copy_from_slice(frame_bytes);
        let total = (NET_HDR + frame_bytes.len()) as u32;

        unsafe {
            // Веха 87: буфер лежит в куче ядра — устройству отдаём ФИЗИЧЕСКИЙ адрес.
            let pa = arch::virt_to_phys(self.tx_buf.as_ptr() as usize) as u64;
            self.tx.set_desc(0, pa, total, 0, 0);
            self.tx.offer(0);
            self.tx.kick();
            // Ждём завершения передачи (синхронно — TX-буфер один).
            while !self.tx.has_used() {
                core::hint::spin_loop();
            }
            self.tx.take_used();
        }
        true
    }

    /// Принять один кадр в `out`, если есть. Возвращает число байт кадра (без заголовка),
    /// либо 0 — приёмное кольцо пусто. Снятый буфер сразу ПЕРЕвыставляется.
    fn recv(&mut self, out: &mut [u8]) -> usize {
        unsafe {
            if !self.rx.has_used() {
                return 0;
            }
            let e = self.rx.take_used();
            let idx = e.id as usize;
            // Длина кадра = записанное устройством минус заголовок virtio_net.
            let flen = (e.len as usize).saturating_sub(NET_HDR).min(out.len());
            if idx < self.rx_bufs.len() {
                out[..flen].copy_from_slice(&self.rx_bufs[idx][NET_HDR..NET_HDR + flen]);
                // Вернуть буфер в приёмное кольцо (дескриптор `id` не менялся).
                self.rx.offer(e.id as u16);
                self.rx.kick();
            }
            flen
        }
    }
}

// Безопасно: все ring-поля — адреса/числа, буферы — Box; доступ под SpinLock.
unsafe impl Send for VirtioNet {}

static NET: SpinLock<Option<VirtioNet>> = SpinLock::new(None);

/// Веха 132.2 — поднялся ли драйвер. Отличать «карты нет» от «карта с нулевым MAC» обязательно:
/// на втором net-srv поднимался вхолостую и уходил спрашивать DHCP у пустоты.
pub fn present() -> bool {
    NET.lock().is_some()
}

/// Согласованная с устройством глубина приёмной очереди (0 — карты нет).
pub fn rx_ring_len() -> usize {
    NET.lock().as_ref().map_or(0, |n| n.rx.size())
}

/// Веха 91 - прерывание ПРИЁМА карты (0 - нет, драйвер остаётся на опросе) и адреса, по которым
/// его подтверждают. Адреса сохраняем заранее: обработчик прерывания не имеет права брать замок
/// `NET`, который держит обычный код (Веха 89, п.1 - иначе дедлок на одном ядре).
static IRQ_NUM: AtomicU32 = AtomicU32::new(0);
static IRQ_ACK_MMIO: AtomicUsize = AtomicUsize::new(0);
static IRQ_ACK_ISR: AtomicUsize = AtomicUsize::new(0);

/// Номер прерывания приёма в терминах арха. 0 - карта без IRQ.
/// На x86 вектор известен заранее (MSI-X), поэтому там эту функцию не спрашивают.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub fn irq() -> u32 {
    IRQ_NUM.load(Ordering::Relaxed)
}

/// Веха 91 — СНЯТЬ висящее прерывание устройства. Нужно ровно один раз, при включении источника
/// в контроллере, и это оказалось решающим на riscv: пока драйвер работал опросом, он никогда не
/// подтверждал `InterruptStatus`, поэтому линия virtio-mmio стояла поднятой с первого же кадра.
/// PLIC ловит ФРОНТ — при уже поднятой линии нового фронта не будет. Сбрасываем статус, и
/// следующий кадр даёт честный фронт. На x86 (MSI-X) висящей линии нет — там не зовётся.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub fn ack_pending() {
    let base = IRQ_ACK_MMIO.load(Ordering::Relaxed);
    if base != 0 {
        unsafe {
            let is = virtio::r32(base, virtio::REG_INTERRUPT_STATUS);
            if is != 0 {
                virtio::w32(base, virtio::REG_INTERRUPT_ACK, is);
            }
        }
    }
}

/// Веха 91 - обработчик прерывания приёма. Работы минимум: подтвердить прерывание устройству и
/// отметить приход кадра. Разбор - дело сетевого сервера в userspace; ядро лишь будит спящего.
pub fn on_irq() {
    let base = IRQ_ACK_MMIO.load(Ordering::Relaxed);
    if base != 0 {
        unsafe {
            let is = virtio::r32(base, virtio::REG_INTERRUPT_STATUS);
            if is != 0 {
                virtio::w32(base, virtio::REG_INTERRUPT_ACK, is);
            }
        }
    }
    let isr = IRQ_ACK_ISR.load(Ordering::Relaxed);
    if isr != 0 {
        unsafe { core::ptr::read_volatile(isr as *const u8) }; // чтение = сброс INTx-статуса
    }
    crate::proc::on_net_irq();
}

/// Инициализировать первую найденную virtio-net (через арх). `false` — карты нет.
///
/// Рукопожатие общее с блоком и энтропией ([`crate::virtio`]). Своего у сети три вещи: бит
/// `VIRTIO_NET_F_MAC` (и его берут, только если устройство его предлагает — иначе конфиг-область
/// не прочтётся), ДВЕ очереди вместо одной и MSI-X-вектор ровно у приёмной: TX-завершения нам не
/// нужны, мы ждём их на месте.
pub fn init() -> bool {
    let Some(dev) = arch::probe_virtio_net() else {
        return false;
    };
    match dev.transport {
        arch::BlkTransport::Mmio { base } => IRQ_ACK_MMIO.store(base, Ordering::Relaxed),
        arch::BlkTransport::Pci { isr, .. } => IRQ_ACK_ISR.store(isr, Ordering::Relaxed),
    }

    let cfg = Cfg::new(dev.transport);
    cfg.begin();
    // MAC берём только тогда, когда его дают: `device_features_lo` у mmio читать нечем, и там
    // бит просят как раньше — безусловно.
    if !cfg.accept_features(cfg.device_features_lo() & FEATURE_LO_MAC) {
        return false;
    }
    cfg.no_config_msix();

    // Вектор просим только у PCI и только приёмной очереди. У mmio прерывание одно на
    // устройство, и назначает его арх.
    let pci = matches!(dev.transport, arch::BlkTransport::Pci { .. });
    let Some(rx) = cfg.queue("virtio-net rx", 0, QSIZE, pci.then_some(0u16)) else {
        return false;
    };
    let Some(tx) = cfg.queue("virtio-net tx", 1, QSIZE, None) else {
        return false;
    };
    cfg.ready();

    let conf = cfg.config();
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((conf + i) as *const u8) };
    }

    publish(rx, tx, mac);
    IRQ_NUM.store(dev.irq, Ordering::Relaxed); // Веха 91: карта умеет будить нас кадром
    true
}

/// Опубликовать готовое устройство под замком и засеять приёмные буферы.
fn publish(rx: Queue, tx: Queue, mac: [u8; 6]) {
    // Буферов ровно столько, сколько слотов согласовано с устройством, — не QSIZE: лишние были бы
    // памятью, в которую никто никогда не напишет.
    let slots = rx.size();
    let mut rx_bufs = Vec::with_capacity(slots);
    for _ in 0..slots {
        rx_bufs.push(Box::new([0u8; BUF]));
    }
    let mut net = VirtioNet { rx, tx, rx_bufs, tx_buf: Box::new([0u8; BUF]), mac };

    // Засеять RX-кольцо: дескриптор i указывает на буфер i (устройство В него ПИШЕТ).
    unsafe {
        for i in 0..slots {
            let pa = arch::virt_to_phys(net.rx_bufs[i].as_ptr() as usize) as u64;
            net.rx.set_desc(i, pa, BUF as u32, DESC_F_WRITE, 0);
            net.rx.offer(i as u16);
        }
    }
    net.rx.kick();

    *NET.lock() = Some(net);
}

/// MAC-адрес карты (0…0, если не инициализирована).
pub fn mac() -> [u8; 6] {
    NET.lock().as_ref().map_or([0u8; 6], |n| n.mac)
}

/// Отправить Ethernet-кадр. `false` — карты нет или кадр слишком велик.
pub fn send(frame_bytes: &[u8]) -> bool {
    match NET.lock().as_mut() {
        Some(n) => n.send(frame_bytes),
        None => false,
    }
}

/// Принять один кадр (неблокирующе). Возвращает число байт (0 — приёмник пуст).
pub fn recv(out: &mut [u8]) -> usize {
    match NET.lock().as_mut() {
        Some(n) => n.recv(out),
        None => 0,
    }
}
