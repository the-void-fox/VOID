//! Выбор сетевого носителя (Веха 49): **e1000**, **virtio-net** — или **карта, живущая в
//! процессе** (Веха 195).
//!
//! Все они дают один интерфейс сырых Ethernet-кадров (`send`/`recv`/`mac`), а стек
//! ARP/IPv4/ICMP живёт в userspace (`net-srv`, Веха 34) — ему всё равно, чья карта. Выбор
//! делает загрузка ([`use_e1000`]): на железе поднялся e1000 — кадры через него, иначе —
//! virtio-net. Зеркало диспетчера носителя store'а ([`crate::object`] AHCI/virtio-blk).
//!
//! ## Третий носитель: устройство в ПРОЦЕССЕ (Веха 195)
//!
//! Хостинг неизменённых драйверов Linux доказан (Вехи 73, 133, 193), но доказан он был в
//! одиночку: драйвер собирал ARP и ICMP прямо в себе, а до настоящего стека кадры не доходили
//! ВОВСЕ — `netif_receive_skb` в шиме их освобождал. То есть широта, ради которой весь конвейер
//! и затевался, упиралась в отсутствие десяти строк: карта есть, стека для неё нет.
//!
//! Здесь этих строк нет по-прежнему — их не нужно. Ядро уже умеет быть посредником между картой
//! и стеком; хватило разрешить, чтобы **картой был процесс**:
//!
//! ```text
//!   net-srv ──net_send──→ ядро[очередь TX] ──netdev_tx_pop──→ драйвер ──→ провод
//!   net-srv ←─net_recv─── ядро[очередь RX] ←─netdev_rx_push── драйвер ←── провод
//! ```
//!
//! `net-srv` не меняется ни на строку и не знает, что карта сменила сторону кольца. Это же
//! отвечает, почему очереди живут в ЯДРЕ, а не разделяемой памятью между двумя процессами:
//! интерфейс «сырые кадры по capability» уже есть, и второй способ передавать кадры пришлось бы
//! объяснять обеим сторонам.
//!
//! Цена — копия кадра на каждом переходе, то есть две вместо одной. Замена — разделяемое кольцо
//! (`shm`), и она напрашивается, когда станет видно на замере; сегодня узкое место не здесь, а
//! в том, что RX у хостируемого драйвера идёт через опрос NAPI в кооперативном планировщике.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::SpinLock;

static E1000_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Переключить сеть на e1000 (зовёт `kmain`, когда `e1000::init()` удался).
pub fn use_e1000() {
    E1000_ACTIVE.store(true, Ordering::Relaxed);
}

#[inline]
fn e1000() -> bool {
    E1000_ACTIVE.load(Ordering::Relaxed)
}

// ─── карта в процессе (Веха 195) ───────────────────────────────────────────────────────────────

/// Сколько кадров ждут в каждой очереди. Приёмное кольцо карты у нас 512 дескрипторов, но здесь
/// счёт другой: это запас на время между двумя заходами `net-srv`, а он опрашивает сеть в своём
/// цикле. Шестнадцати хватает с избытком; глубже — значит дольше держать устаревшие кадры, что
/// для TCP хуже, чем потерять их сразу (правило «лучше отбросить, чем задержать»).
const QLEN: usize = 16;
/// Ethernet-кадр: 1514 байта максимум без VLAN. Округлено вверх с запасом на тег.
const FRAME: usize = 1600;

/// Кольцо кадров. Обычная очередь на массиве: кадры короткие, а копия их всё равно неизбежна
/// (страницы процесса не отображены в ядро).
struct Ring {
    buf: [[u8; FRAME]; QLEN],
    len: [u16; QLEN],
    head: usize,
    tail: usize,
    /// Сколько кадров отброшено переполнением. Счётчик, а не флаг: важно не «была потеря», а
    /// растёт ли она (см. [`ext_rx_push`] — о потерях говорится вслух).
    dropped: u32,
}

impl Ring {
    const fn new() -> Self {
        Ring { buf: [[0; FRAME]; QLEN], len: [0; QLEN], head: 0, tail: 0, dropped: 0 }
    }

    fn push(&mut self, f: &[u8]) -> bool {
        if f.len() > FRAME {
            self.dropped += 1;
            return false;
        }
        let next = (self.tail + 1) % QLEN;
        if next == self.head {
            self.dropped += 1;
            return false;
        }
        self.buf[self.tail][..f.len()].copy_from_slice(f);
        self.len[self.tail] = f.len() as u16;
        self.tail = next;
        true
    }

    fn pop(&mut self, out: &mut [u8]) -> usize {
        if self.head == self.tail {
            return 0;
        }
        let n = (self.len[self.head] as usize).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.head][..n]);
        self.head = (self.head + 1) % QLEN;
        n
    }
}

/// Кадры от драйвера к стеку.
static RX: SpinLock<Ring> = SpinLock::new(Ring::new());
/// Кадры от стека к драйверу.
static TX: SpinLock<Ring> = SpinLock::new(Ring::new());
/// MAC карты, как её назвал драйвер.
static EXT_MAC: SpinLock<[u8; 6]> = SpinLock::new([0; 6]);
/// Процесс-драйвер: [`usize::MAX`] — карты в процессе нет. Хранится ради ПРОБУЖДЕНИЯ: иначе
/// драйвер узнавал бы про исходящий кадр только на следующем своём таймере, то есть через
/// десятки миллисекунд, а опрашивать очередь вхолостую — жечь процессор в простое.
static EXT_OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Драйвер объявил себя картой системы. Зовётся из `SYS_NETDEV` (op «представиться»).
pub fn ext_attach(pid: usize, mac: [u8; 6]) {
    *EXT_MAC.lock() = mac;
    EXT_OWNER.store(pid, Ordering::Release);
    crate::println!(
        "  [net] карта в процессе P{}: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} — кадры идут в стек",
        pid, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
    );
}

/// Драйвер ушёл (умер или отсоединился): карты снова нет. Кадры в очередях больше никому не
/// нужны — стек, получив «карты нет», начинает сначала.
pub fn ext_detach(pid: usize) {
    if EXT_OWNER.compare_exchange(pid, usize::MAX, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        *RX.lock() = Ring::new();
        *TX.lock() = Ring::new();
        crate::println!("  [net] карта в процессе P{} отсоединилась", pid);
    }
}

/// Кто держит карту (`None` — никто).
pub fn ext_owner() -> Option<usize> {
    match EXT_OWNER.load(Ordering::Acquire) {
        usize::MAX => None,
        pid => Some(pid),
    }
}

#[inline]
fn ext() -> bool {
    EXT_OWNER.load(Ordering::Acquire) != usize::MAX
}

/// Драйвер принял кадр с провода. `false` — очередь полна и кадр отброшен.
pub fn ext_rx_push(f: &[u8]) -> bool {
    let mut g = RX.lock();
    if g.push(f) {
        return true;
    }
    // Потеря кадров — нормальная работа сети, но ТИХАЯ потеря превращается в «сеть иногда
    // тормозит» без единой зацепки. Говорим о первой и дальше каждой 64-й: журнал переживёт.
    let n = g.dropped;
    drop(g);
    if n == 1 || n % 64 == 0 {
        crate::println!("  [net] приёмная очередь полна — кадров потеряно {}", n);
    }
    false
}

/// Драйвер забирает кадр, который стек просил отправить. 0 — отправлять нечего.
pub fn ext_tx_pop(out: &mut [u8]) -> usize {
    TX.lock().pop(out)
}



/// Веха 132.2 — есть ли у ЯДРА работающая карта. Нужен там, где «нет карты» и «карта с нулевым
/// MAC» обязаны различаться: без этого `net-srv` бодро поднимался с адресом 00:00:00:00:00:00 и
/// уходил спрашивать DHCP у пустоты, а система в одном логе сообщала и «сетевой карты нет», и
/// «сеть запущена». Владелец справедливо на это указал.
pub fn present() -> bool {
    if ext() {
        return true;
    }
    if e1000() {
        crate::e1000::present()
    } else {
        crate::virtio_net::present()
    }
}

/// MAC активной карты.
pub fn mac() -> [u8; 6] {
    if ext() {
        return *EXT_MAC.lock();
    }
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
    if ext() {
        return QLEN;
    }
    if e1000() {
        crate::e1000::rx_ring_len()
    } else {
        crate::virtio_net::rx_ring_len()
    }
}

/// Отправить Ethernet-кадр активной картой.
pub fn send(frame_bytes: &[u8]) -> bool {
    if ext() {
        // Кадр только КЛАДЁТСЯ в очередь: отправляет его драйвер, и разбудить его — дело
        // вызывающего (`proc::wake_netdev_owner`), у которого в руках таблица процессов.
        return TX.lock().push(frame_bytes);
    }
    if e1000() {
        crate::e1000::send(frame_bytes)
    } else {
        crate::virtio_net::send(frame_bytes)
    }
}

/// Принять один кадр (неблокирующе) активной картой. 0 — приёмник пуст.
pub fn recv(out: &mut [u8]) -> usize {
    if ext() {
        return RX.lock().pop(out);
    }
    if e1000() {
        crate::e1000::recv(out)
    } else {
        crate::virtio_net::recv(out)
    }
}
