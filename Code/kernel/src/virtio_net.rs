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

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicU32, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::{arch, frame};

/// Размер очереди (число дескрипторов/буферов). RX-буферов столько же.
const QSIZE: usize = 8;
/// Размер одного буфера: 12-байтный заголовок + кадр (Ethernet MTU 1514 + запас).
const BUF: usize = 2048;
/// Длина заголовка virtio_net_hdr (modern, VIRTIO_F_VERSION_1 → есть num_buffers).
const NET_HDR: usize = 12;

// Регистры virtio-mmio (те же смещения, что у блока; magic/version/id проверил арх).
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
/// Веха 91 - статус прерывания и подтверждение (virtio-mmio; смещения те же, что у blk).
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const REG_CONFIG: usize = 0x100; // конфиг устройства: MAC (6 байт) при VIRTIO_NET_F_MAC

// Поля common_cfg virtio-pci modern.
const PCI_DEVICE_FEATURE_SEL: usize = 0x00;
const PCI_DEVICE_FEATURE: usize = 0x04;
const PCI_DRIVER_FEATURE_SEL: usize = 0x08;
const PCI_DRIVER_FEATURE: usize = 0x0c;
const PCI_MSIX_CONFIG: usize = 0x10;
const PCI_DEVICE_STATUS: usize = 0x14;
const PCI_QUEUE_SEL: usize = 0x16;
const PCI_QUEUE_SIZE: usize = 0x18;
const PCI_QUEUE_MSIX_VECTOR: usize = 0x1a;
const PCI_QUEUE_ENABLE: usize = 0x1c;
const PCI_QUEUE_NOTIFY_OFF: usize = 0x1e;
const PCI_QUEUE_DESC: usize = 0x20;
const PCI_QUEUE_DRIVER: usize = 0x28;
const PCI_QUEUE_DEVICE: usize = 0x30;
const PCI_NO_VECTOR: u16 = 0xffff;

// Биты Status.
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

// Флаг дескриптора (цепочек у нас нет — по одному дескриптору на кадр).
const DESC_F_WRITE: u16 = 2; // устройство ПИШЕТ в буфер (для нас — приём кадра)

// Feature-биты (dword 0): VIRTIO_NET_F_MAC = 5. Dword 1: VIRTIO_F_VERSION_1 = бит 0 (32).
const FEATURE_LO_MAC: u32 = 1 << 5;
const FEATURE_HI_VERSION_1: u32 = 1;

#[allow(dead_code)]
#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[allow(dead_code)]
#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
    used_event: u16,
}

#[allow(dead_code)]
#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[allow(dead_code)]
#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
    avail_event: u16,
}

/// Как «дёргать» очередь (notify). У каждой очереди на PCI свой адрес; на mmio — общий
/// регистр, в который пишут номер очереди.
enum Notify {
    Mmio { base: usize },
    Pci { rx: usize, tx: usize },
}

impl Notify {
    fn kick(&self, queue: u16) {
        unsafe {
            match self {
                Notify::Mmio { base } => w32(*base, REG_QUEUE_NOTIFY, queue as u32),
                Notify::Pci { rx, tx } => {
                    let addr = if queue == 0 { *rx } else { *tx };
                    write_volatile(addr as *mut u16, queue);
                }
            }
        }
    }
}

/// Одна очередь: адреса трёх колец и теневые индексы.
struct Queue {
    desc: usize,
    avail: usize,
    used: usize,
    /// Наш продюсер-индекс avail (сколько всего опубликовали).
    avail_idx: u16,
    /// Сколько записей used уже обработали.
    used_idx: u16,
}

impl Queue {
    /// Опубликовать дескриптор `head` в avail и увеличить idx (без notify).
    unsafe fn publish(&mut self, head: u16) {
        let avail = crate::frame::ptr(self.avail) as *mut Avail;
        write_volatile(&mut (*avail).ring[(self.avail_idx as usize) % QSIZE], head);
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        write_volatile(&mut (*avail).idx, self.avail_idx);
        fence(Ordering::SeqCst);
    }

    /// Есть ли необработанные записи в used.
    unsafe fn has_used(&self) -> bool {
        let used = crate::frame::ptr(self.used) as *const Used;
        read_volatile(&(*used).idx) != self.used_idx
    }

    /// Снять следующую запись used: (id дескриптора, число байт от устройства).
    unsafe fn take_used(&mut self) -> (u16, u32) {
        let used = crate::frame::ptr(self.used) as *const Used;
        let slot = (self.used_idx as usize) % QSIZE;
        let e = &(*used).ring[slot];
        let (id, len) = (read_volatile(&e.id) as u16, read_volatile(&e.len));
        fence(Ordering::SeqCst);
        self.used_idx = self.used_idx.wrapping_add(1);
        (id, len)
    }
}

/// Инициализированная сетевая карта.
struct VirtioNet {
    notify: Notify,
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

        let desc = crate::frame::ptr(self.tx.desc) as *mut Desc;
        unsafe {
            // Веха 87: буфер лежит в куче ядра — устройству отдаём ФИЗИЧЕСКИЙ адрес.
            let pa = arch::virt_to_phys(self.tx_buf.as_ptr() as usize) as u64;
            set_desc(desc, 0, pa, total, 0, 0);
            fence(Ordering::SeqCst);
            self.tx.publish(0);
            self.notify.kick(1);
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
            let (id, len) = self.rx.take_used();
            let idx = id as usize;
            // Длина кадра = записанное устройством минус заголовок virtio_net.
            let flen = (len as usize).saturating_sub(NET_HDR).min(out.len());
            if idx < self.rx_bufs.len() {
                out[..flen].copy_from_slice(&self.rx_bufs[idx][NET_HDR..NET_HDR + flen]);
                // Вернуть буфер в приёмное кольцо (дескриптор `id` не менялся).
                self.rx.publish(id);
                self.notify.kick(0);
            }
            flen
        }
    }
}

// Безопасно: все ring-поля — адреса/числа, буферы — Box; доступ под SpinLock.
unsafe impl Send for VirtioNet {}

static NET: SpinLock<Option<VirtioNet>> = SpinLock::new(None);

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
            let is = r32(base, REG_INTERRUPT_STATUS);
            if is != 0 {
                w32(base, REG_INTERRUPT_ACK, is);
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
            let is = r32(base, REG_INTERRUPT_STATUS);
            if is != 0 {
                w32(base, REG_INTERRUPT_ACK, is);
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
pub fn init() -> bool {
    let Some(dev) = arch::probe_virtio_net() else {
        return false;
    };
    let ok = match dev.transport {
        arch::BlkTransport::Mmio { base } => {
            IRQ_ACK_MMIO.store(base, Ordering::Relaxed);
            init_mmio(base)
        }
        arch::BlkTransport::Pci { common, notify_base, notify_mult, isr, device } => {
            IRQ_ACK_ISR.store(isr, Ordering::Relaxed);
            init_pci(common, notify_base, notify_mult, device)
        }
    };
    if ok {
        IRQ_NUM.store(dev.irq, Ordering::Relaxed); // Веха 91: карта умеет будить нас кадром
    }
    ok
}

/// Настроить одну очередь через mmio: выбрать, задать размер, отдать адреса колец, READY.
/// Возвращает готовую [`Queue`] или `None`, если очереди у устройства нет.
unsafe fn setup_queue_mmio(base: usize, q: u16) -> Option<Queue> {
    w32(base, REG_QUEUE_SEL, q as u32);
    if r32(base, REG_QUEUE_NUM_MAX) == 0 {
        return None;
    }
    w32(base, REG_QUEUE_NUM, QSIZE as u32);
    let (desc, avail, used) = alloc_rings();
    write_addr(base, REG_QUEUE_DESC_LOW, REG_QUEUE_DESC_HIGH, desc);
    write_addr(base, REG_QUEUE_DRIVER_LOW, REG_QUEUE_DRIVER_HIGH, avail);
    write_addr(base, REG_QUEUE_DEVICE_LOW, REG_QUEUE_DEVICE_HIGH, used);
    w32(base, REG_QUEUE_READY, 1);
    Some(Queue { desc, avail, used, avail_idx: 0, used_idx: 0 })
}

fn init_mmio(base: usize) -> bool {
    unsafe {
        w32(base, REG_STATUS, 0);
        let mut status = STATUS_ACKNOWLEDGE;
        w32(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        w32(base, REG_STATUS, status);

        // Фичи: VIRTIO_NET_F_MAC (dword 0) + VIRTIO_F_VERSION_1 (dword 1).
        w32(base, REG_DRIVER_FEATURES_SEL, 0);
        w32(base, REG_DRIVER_FEATURES, FEATURE_LO_MAC);
        w32(base, REG_DRIVER_FEATURES_SEL, 1);
        w32(base, REG_DRIVER_FEATURES, FEATURE_HI_VERSION_1);
        status |= STATUS_FEATURES_OK;
        w32(base, REG_STATUS, status);
        if r32(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
            return false;
        }

        let Some(rx) = setup_queue_mmio(base, 0) else { return false };
        let Some(tx) = setup_queue_mmio(base, 1) else { return false };

        status |= STATUS_DRIVER_OK;
        w32(base, REG_STATUS, status);

        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = read_volatile((base + REG_CONFIG + i) as *const u8);
        }

        publish(Notify::Mmio { base }, rx, tx, mac);
    }
    true
}

fn init_pci(common: usize, notify_base: usize, notify_mult: u32, device: usize) -> bool {
    let r8p = |off: usize| unsafe { read_volatile((common + off) as *const u8) };
    let w8p = |off: usize, v: u8| unsafe { write_volatile((common + off) as *mut u8, v) };
    let r16p = |off: usize| unsafe { read_volatile((common + off) as *const u16) };
    let w16p = |off: usize, v: u16| unsafe { write_volatile((common + off) as *mut u16, v) };
    let r32p = |off: usize| unsafe { read_volatile((common + off) as *const u32) };
    let w32p = |off: usize, v: u32| unsafe { write_volatile((common + off) as *mut u32, v) };
    let w64p = |off: usize, v: u64| unsafe { write_volatile((common + off) as *mut u64, v) };

    w8p(PCI_DEVICE_STATUS, 0);
    let mut status = STATUS_ACKNOWLEDGE as u8;
    w8p(PCI_DEVICE_STATUS, status);
    status |= STATUS_DRIVER as u8;
    w8p(PCI_DEVICE_STATUS, status);

    // Убедиться, что устройство предлагает MAC (dword 0) — иначе конфиг MAC не прочтём.
    w32p(PCI_DEVICE_FEATURE_SEL, 0);
    let dev_lo = r32p(PCI_DEVICE_FEATURE);
    w32p(PCI_DRIVER_FEATURE_SEL, 0);
    w32p(PCI_DRIVER_FEATURE, dev_lo & FEATURE_LO_MAC);
    w32p(PCI_DRIVER_FEATURE_SEL, 1);
    w32p(PCI_DRIVER_FEATURE, FEATURE_HI_VERSION_1);
    status |= STATUS_FEATURES_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);
    if r8p(PCI_DEVICE_STATUS) & STATUS_FEATURES_OK as u8 == 0 {
        return false;
    }

    // Конфиг-события не нужны.
    w16p(PCI_MSIX_CONFIG, PCI_NO_VECTOR);

    // Настроить очереди 0 (RX) и 1 (TX): размер, кольца, notify-адрес, без MSI-X.
    let mut notify = [0usize; 2];
    let mut queues: [Option<Queue>; 2] = [None, None];
    for q in 0..2u16 {
        w16p(PCI_QUEUE_SEL, q);
        if r16p(PCI_QUEUE_SIZE) == 0 {
            return false;
        }
        w16p(PCI_QUEUE_SIZE, QSIZE as u16);
        // Веха 91 - привязать очередь ПРИЁМА (q=0) к записи 0 таблицы MSI-X. Без этого
        // устройство остаётся с NO_VECTOR и прерываний не шлёт вовсе: ровно этот шаг делает
        // virtio-blk, и ровно его тут не хватало - сеть поэтому и жила опросом.
        // Очередь передачи (q=1) вектора не получает: TX-завершения нам не нужны.
        w16p(PCI_QUEUE_MSIX_VECTOR, if q == 0 { 0 } else { PCI_NO_VECTOR });
        let (desc, avail, used) = alloc_rings();
        w64p(PCI_QUEUE_DESC, desc as u64);
        w64p(PCI_QUEUE_DRIVER, avail as u64);
        w64p(PCI_QUEUE_DEVICE, used as u64);
        notify[q as usize] =
            notify_base + r16p(PCI_QUEUE_NOTIFY_OFF) as usize * notify_mult as usize;
        w16p(PCI_QUEUE_ENABLE, 1);
        queues[q as usize] = Some(Queue { desc, avail, used, avail_idx: 0, used_idx: 0 });
    }

    status |= STATUS_DRIVER_OK as u8;
    w8p(PCI_DEVICE_STATUS, status);

    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = unsafe { read_volatile((device + i) as *const u8) };
    }

    let rx = queues[0].take().unwrap();
    let tx = queues[1].take().unwrap();
    publish(Notify::Pci { rx: notify[0], tx: notify[1] }, rx, tx, mac);
    true
}

/// Три кольца очереди — в обнулённых фреймах (RAM идентична: адрес = физический).
fn alloc_rings() -> (usize, usize, usize) {
    (
        frame::alloc().expect("virtio-net desc"),
        frame::alloc().expect("virtio-net avail"),
        frame::alloc().expect("virtio-net used"),
    )
}

/// Опубликовать готовое устройство под замком и засеять приёмные буферы.
fn publish(notify: Notify, rx: Queue, tx: Queue, mac: [u8; 6]) {
    let mut rx_bufs = Vec::with_capacity(QSIZE);
    for _ in 0..QSIZE {
        rx_bufs.push(Box::new([0u8; BUF]));
    }
    let mut net = VirtioNet { notify, rx, tx, rx_bufs, tx_buf: Box::new([0u8; BUF]), mac };

    // Засеять RX-кольцо: дескриптор i указывает на буфер i (устройство В него ПИШЕТ).
    unsafe {
        let desc = crate::frame::ptr(net.rx.desc) as *mut Desc;
        for i in 0..QSIZE {
            let pa = arch::virt_to_phys(net.rx_bufs[i].as_ptr() as usize) as u64;
            set_desc(desc, i, pa, BUF as u32, DESC_F_WRITE, 0);
            net.rx.publish(i as u16);
        }
    }
    net.notify.kick(0);

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

// ─── низкоуровневые помощники ────────────────────────────────────────────────

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn w32(base: usize, off: usize, val: u32) {
    write_volatile((base + off) as *mut u32, val);
}

#[inline]
unsafe fn write_addr(base: usize, lo: usize, hi: usize, addr: usize) {
    w32(base, lo, addr as u32);
    w32(base, hi, (addr as u64 >> 32) as u32);
}

#[inline]
unsafe fn set_desc(desc: *mut Desc, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
    let d = desc.add(i);
    write_volatile(&mut (*d).addr, addr);
    write_volatile(&mut (*d).len, len);
    write_volatile(&mut (*d).flags, flags);
    write_volatile(&mut (*d).next, next);
}
