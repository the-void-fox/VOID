//! Выбор сетевого носителя (Веха 49): **e1000** (реальная карта) или **virtio-net** (QEMU).
//!
//! Оба драйвера дают один интерфейс сырых Ethernet-кадров (`send`/`recv`/`mac`), а стек
//! ARP/IPv4/ICMP живёт в userspace (`net-srv`, Веха 34) — ему всё равно, чья карта. Выбор
//! делает загрузка ([`use_e1000`]): на железе поднялся e1000 — кадры через него, иначе —
//! virtio-net. Зеркало диспетчера носителя store'а ([`crate::object`] AHCI/virtio-blk).

use core::sync::atomic::{AtomicBool, Ordering};

static E1000_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Переключить сеть на e1000 (зовёт `kmain`, когда `e1000::init()` удался).
pub fn use_e1000() {
    E1000_ACTIVE.store(true, Ordering::Relaxed);
}

#[inline]
fn e1000() -> bool {
    E1000_ACTIVE.load(Ordering::Relaxed)
}

/// Веха 132.2 — есть ли у ЯДРА работающая карта. Нужен там, где «нет карты» и «карта с нулевым
/// MAC» обязаны различаться: без этого `net-srv` бодро поднимался с адресом 00:00:00:00:00:00 и
/// уходил спрашивать DHCP у пустоты, а система в одном логе сообщала и «сетевой карты нет», и
/// «сеть запущена». Владелец справедливо на это указал.
pub fn present() -> bool {
    if e1000() {
        crate::e1000::present()
    } else {
        crate::virtio_net::present()
    }
}

/// MAC активной карты.
pub fn mac() -> [u8; 6] {
    if e1000() {
        crate::e1000::mac()
    } else {
        crate::virtio_net::mac()
    }
}

/// Сколько кадров держит приёмное кольцо активной карты, пока их не разобрали (Веха 135.2).
///
/// Это не справочная величина: по ней userspace-стек объявляет окно TCP. У virtio размер
/// СОГЛАСУЕТСЯ с устройством и может выйти меньше запрошенного, поэтому спрашиваем карту, а не
/// константу.
pub fn rx_ring_len() -> usize {
    if e1000() {
        crate::e1000::rx_ring_len()
    } else {
        crate::virtio_net::rx_ring_len()
    }
}

/// Отправить Ethernet-кадр активной картой.
pub fn send(frame_bytes: &[u8]) -> bool {
    if e1000() {
        crate::e1000::send(frame_bytes)
    } else {
        crate::virtio_net::send(frame_bytes)
    }
}

/// Принять один кадр (неблокирующе) активной картой. 0 — приёмник пуст.
pub fn recv(out: &mut [u8]) -> usize {
    if e1000() {
        crate::e1000::recv(out)
    } else {
        crate::virtio_net::recv(out)
    }
}
