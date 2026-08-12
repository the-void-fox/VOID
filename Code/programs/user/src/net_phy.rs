//! Веха 90 — мост между smoltcp и VOID: устройство `phy::Device` поверх сырых кадров ядра.
//!
//! Ядро по-прежнему не знает о протоколах (микроядерность, [[0008-network-stack]]): оно отдаёт
//! кадры целиком через `SYS_NET_SEND`/`SYS_NET_RECV`, а весь стек живёт здесь, в userspace.
//! Этот файл — вся граница между чужим кодом стека и нашей системой; он намеренно тонкий.
//!
//! Буферы статические: smoltcp собран без `alloc` (см. `Cargo.toml`), поэтому кадры лежат
//! прямо в токенах, а не в куче. MTU 1500 — обычный Ethernet.

use smoltcp::phy::{self, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Максимальный кадр Ethernet (без FCS — её снимает карта).
pub const MTU: usize = 1514;

/// Сетевое устройство VOID: capability на карту + приёмный буфер под один кадр.
pub struct VoidDevice {
    dev_cap: usize,
    rx: [u8; MTU],
}

impl VoidDevice {
    pub fn new(dev_cap: usize) -> Self {
        Self { dev_cap, rx: [0; MTU] }
    }
}

/// Веха 90 — время для стека. smoltcp хочет монотонные МИЛЛИСЕКУНДЫ, а ядро отдаёт тики
/// (`rdtime`/`rdtsc`, цена тика — `TICK_NS`, разная у арх). Переводим здесь, чтобы у стека не
/// было ни одного представления о нашем железе.
pub fn now() -> Instant {
    Instant::from_millis(((crate::now() as u64).wrapping_mul(crate::TICK_NS as u64) / 1_000_000) as i64)
}

impl phy::Device for VoidDevice {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let n = crate::net_recv(self.dev_cap, &mut self.rx);
        // 0 — кадров нет; MAX — карты нет (шлюз ядра так сообщает об отсутствии устройства).
        if n == 0 || n == usize::MAX || n > MTU {
            return None;
        }
        Some((RxToken { frame: &self.rx[..n] }, TxToken { dev_cap: self.dev_cap }))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { dev_cap: self.dev_cap })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = MTU;
        // Сколько кадров подряд наша сторона переживёт, не потеряв ни одного. Веха 135.
        //
        // Стояла ЕДИНИЦА — по рассуждению «кадры ходят через шлюз ядра по одному». Рассуждение
        // про API, а число значит другое: сколько кадров успеет полежать, пока мы не разгребли.
        // А smoltcp по нему ЗАЖИМАЕТ ОБЪЯВЛЯЕМОЕ ОКНО TCP (iface/packet.rs: окно режется до
        // `max_burst_size × (MTU − заголовки)`). С единицей окно навсегда 1474 байта, сколько бы
        // ни было буфера приёма: TCP превращается в «шаг-и-жди» — по одному сегменту за круг.
        //
        // Локально это не видно (круг 0.3 мс), а на настоящей сети решает всё. Замерено на стенде
        // при задержке 15 мс: 8 КиБ наливались 2003 мс шестью кругами.
        //
        // Правда такова: кольца приёма в ядре — 16 дескрипторов у e1000 и 8 у virtio-net, и у
        // каждого свой буфер-кадр. Берём меньшее из двух: какой драйвер под шлюзом, отсюда не
        // видно, а завышать нельзя — завышенное окно означает потерянные кадры вместо медленных.
        c.max_burst_size = Some(8);
        c
    }
}

/// Принятый кадр: живёт в буфере устройства, копий нет.
pub struct RxToken<'a> {
    frame: &'a [u8],
}

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.frame)
    }
}

/// Право отправить один кадр: стек строит его прямо в нашем буфере, мы отдаём ядру.
pub struct TxToken {
    dev_cap: usize,
}

impl phy::TxToken for TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; MTU];
        let n = len.min(MTU);
        let r = f(&mut buf[..n]);
        crate::net_send(self.dev_cap, &buf[..n]);
        r
    }
}
