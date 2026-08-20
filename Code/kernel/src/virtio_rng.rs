//! virtio-rng — аппаратный источник случайности от гипервизора (долг Вехи 86, закрыт перед 95).
//!
//! **Зачем именно сейчас.** До этого драйвера `arch::hw_random_u64()` на riscv возвращал
//! буквально `None`: у QEMU virt нет ни `Zkr`, ни другого источника, поэтому вся случайность
//! шла из пула джиттера прерываний ([`crate::random`]). На свежей загрузке, до накопления
//! событий, такая энтропия предсказуема настолько, насколько предсказуемо время загрузки — а
//! для TLS (Веха 95) это означает предсказуемые ключи, то есть TLS понарошку. На x86 роль
//! источника играл `RDRAND`, и там всё было честно; virtio-rng выравнивает арх.
//!
//! **Устройство простейшее из всех virtio:** одна очередь, никакого протокола. Драйвер кладёт в
//! неё ПИШУЩИЙ буфер, дёргает notify — устройство заполняет его энтропией и отдаёт в used с
//! фактической длиной (может быть меньше запрошенного, это нормально). Прерывания не нужны:
//! запросы редкие и синхронные, ждём в used-кольце.
//!
//! Транспорт — тот же split-virtqueue, что у блока и сети (см. [[virtio-blk]]); отличий два:
//! очередь одна и дескриптор всегда с `DESC_F_WRITE`.

use alloc::boxed::Box;

use crate::arch;
use crate::sync::SpinLock;
use crate::virtio::{Cfg, Queue, DESC_F_WRITE};

/// Длина кольца. Нам хватает одного дескриптора за раз, но кольцо меньше 2 некрасиво и
/// упирается в требования выравнивания — берём 4.
const QSIZE: usize = 4;
/// Сколько байт просим у устройства за один поход. 64 байта = восемь выдач `u64` из одного
/// обращения: устройство трогаем редко, а свежесть сохраняем.
const BUF: usize = 64;

struct VirtioRng {
    q: Queue,
    buf: Box<[u8; BUF]>,
    /// Кэш выданного устройством: `[pos, len)` ещё не отдано наружу.
    cache: [u8; BUF],
    pos: usize,
    len: usize,
}

impl VirtioRng {
    /// Сходить к устройству за свежей порцией. `false` — устройство не отдало ничего.
    fn refill(&mut self) -> bool {
        unsafe {
            // Веха 87: буфер в куче ядра — устройству отдаём ФИЗИЧЕСКИЙ адрес.
            let pa = arch::virt_to_phys(self.buf.as_ptr() as usize) as u64;
            let slot = self.q.slot();
            self.q.set_desc(slot, pa, BUF as u32, DESC_F_WRITE, 0);
            self.q.offer(slot as u16);
            self.q.kick();

            // Ждём завершения. Потолок оборотов — чтобы молчащее устройство не подвесило ядро:
            // энтропия важна, но не ценой зависшей загрузки (пул джиттера останется запасным).
            let mut spins = 0u32;
            while !self.q.has_used() {
                spins += 1;
                if spins > 10_000_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
            let n = (self.q.take_used().len as usize).min(BUF);
            if n == 0 {
                return false;
            }
            self.cache[..n].copy_from_slice(&self.buf[..n]);
            self.pos = 0;
            self.len = n;
            true
        }
    }

    /// Выдать 8 байт энтропии. `None` — устройство молчит.
    fn next_u64(&mut self) -> Option<u64> {
        if self.len - self.pos < 8 && !self.refill() {
            return None;
        }
        if self.len - self.pos < 8 {
            return None;
        }
        let mut w = [0u8; 8];
        w.copy_from_slice(&self.cache[self.pos..self.pos + 8]);
        self.pos += 8;
        Some(u64::from_le_bytes(w))
    }
}

static RNG: SpinLock<Option<VirtioRng>> = SpinLock::new(None);

/// Инициализировать первое найденное virtio-rng. `false` — устройства нет.
pub fn init() -> bool {
    let Some(transport) = arch::probe_virtio_rng() else {
        return false;
    };
    // Рукопожатие общее с блоком и сетью ([`crate::virtio`]); своих feature-битов у virtio-rng
    // нет вовсе, прерывания не нужны — ждём в кольце.
    let cfg = Cfg::new(transport);
    cfg.begin();
    if !cfg.accept_features(0) {
        return false;
    }
    cfg.no_config_msix();
    let Some(q) = cfg.queue("virtio-rng", 0, QSIZE, None) else {
        return false;
    };
    cfg.ready();

    let mut dev = VirtioRng {
        q,
        buf: Box::new([0u8; BUF]),
        cache: [0u8; BUF],
        pos: 0,
        len: 0,
    };
    // Первая порция прямо здесь: заодно проверяем, что устройство отвечает, а не только
    // отрапортовало о готовности.
    if !dev.refill() {
        return false;
    }
    *RNG.lock() = Some(dev);
    true
}

/// Есть ли рабочий источник.
pub fn present() -> bool {
    RNG.lock().is_some()
}

/// Слово энтропии от устройства. `None` — устройства нет или оно молчит.
///
/// Замок здесь `lock_irq`: [`crate::random::fill`] зовётся из syscall-контекста, но пул
/// подмешивается из обработчиков прерываний — брать обычный замок значило бы позволить
/// прерыванию встрять посреди работы с кольцом.
pub fn next_u64() -> Option<u64> {
    RNG.lock_irq().as_mut()?.next_u64()
}
