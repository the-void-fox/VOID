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

/// MAC активной карты.
pub fn mac() -> [u8; 6] {
    if e1000() {
        crate::e1000::mac()
    } else {
        crate::virtio_net::mac()
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
