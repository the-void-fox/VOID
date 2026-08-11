//! Веха 24 — граница архитектур: узкий контракт, за которым живёт ВСЁ арх-специфичное.
//!
//! Общий код ядра (процессы, IPC, store, capability, планировщики, драйвер virtio) не знает
//! ни про CSR, ни про satp, ни про PLIC — только про имена ниже. Каждая архитектура реализует
//! контракт своим модулем (`riscv64/`, `x86_64/`), выбор — на этапе компиляции по
//! `target_arch` (`cargo build --target …` — один проект, N образов). Никаких трейтов и
//! динамики: реализация одна на сборку, диспетчеризация не нужна, всё инлайнится.
//!
//! ## Контракт (что обязан отдать каждый arch)
//!
//! - **Загрузка**: ассемблерная точка входа, зовущая `kmain(hartid, boot_info)`.
//! - **Консоль**: [`Console`] (`core::fmt::Write`), приём: `console_init_rx` /
//!   `console_drain` / `console_getc` / `console_has_input`.
//! - **Прерывания**: `irq_save_disable`/`irq_restore` (критические секции),
//!   `enable_interrupts`, маски сессий процессов: `irq_mask_read`/`irq_mask_write` (снимок)
//!   + политики `irq_mask_preempt` (таймер вкл, устройства выкл — сессия процессов) и
//!   `irq_mask_stdin` (устройства вкл, таймер выкл — сон до ввода); `wait_for_interrupt`;
//!   `init_device_interrupts` (контроллер + маршрутизация IRQ диска/консоли);
//!   `mark_in_kernel` (инвариант «trap пришёл из ядра» после возврата из сессии).
//! - **Таймер**: `timer_hw_init` (размаскировать и включить), `timer_arm` (перевзвести квант
//!   вытеснения; величина кванта — дело арха: таймбазы разные).
//! - **direct-map** (Веха 87): `KERNEL_OFFSET` — смещение отображения RAM, `phys_to_virt` /
//!   `virt_to_phys` — перевод «физический адрес ↔ указатель ядра». Всё ядро обращается к
//!   физической памяти ТОЛЬКО через них, поэтому переезд ядра в верхнюю половину сводится к
//!   смене константы и пути загрузки ([[0010-address-space-layout]]).
//! - **Память**: `mm_init() -> корень`, `mm_enable`, `clone_kernel_root`, `map` с флагами
//!   `MAP_R/W/X/U`, `translate`, `flush_tlb`, токены адресных пространств
//!   `space_token(корень)`/`space_root(токен)` (на RISC-V токен = значение satp), `MM_NAME`;
//!   `unmap_shared` — снять отображение ОДНОЙ общей страницы (Веха 129, [[shm]]).
//! - **Trap'ы**: `trap_init`; [`TrapFrame`] — снимок регистров с методами вместо голых
//!   индексов (`syscall_num`, `arg(i)`, `set_ret`/`set_ret_at`, `set_start_arg`, `advance`,
//!   `restart` — перевзвести блокирующий syscall на повтор: на riscv sepc и так стоит на
//!   `ecall`, на x86 нужен откат rip за `int 0x80`, `new_user`); арх сам классифицирует
//!   trap из U-mode в [`UserTrap`] и зовёт `proc::handle_user_trap(frame, trap)`;
//!   `enter_user` — вход в процесс.
//! - **Контексты**: [`Context`] (opaque: `EMPTY`/`new_task`/`new_kernel`), `context_switch`.
//! - **Устройства**: `probe_virtio_blk() -> Option<BlkDevice>` — найти virtio-blk на шине
//!   СВОЕЙ архитектуры (virtio-mmio у QEMU virt, virtio-pci у q35) и отдать транспорт
//!   ([`BlkTransport`]) общему драйверу; маршрутизацию IRQ устройства арх берёт на себя.
//! - **Часы и случайность** (Веха 86): `wall_clock_unix_ns` — настенное время от прошивки
//!   (`None`, если часов нет); `hw_random_u64` — аппаратный ГСЧ (`None`, если его нет).
//! - **Разное**: `ELF_MACHINE` (e_machine загружаемых программ), `ARCH_NAME` (арх-измерение
//!   корней программ `bin/<arch>/<имя>`), `RAM_LIMIT` (конец RAM платформы — для арены
//!   фреймов), `USERSPACE_READY` (false на архе в bring-up: kmain пропускает процессные
//!   демо, пока не готовы вход в U-mode и программы), `power_off`.
//!
//! Платформенные константы (адреса RAM/MMIO QEMU virt) пока остаются в общих
//! `frame`/`virtio_blk` — их черёд отделяться придёт с реальным x86-железом (Вехи 25+),
//! когда появится слой «платформа» поверх слоя «архитектура»; см. ADR 0005.

#[cfg(target_arch = "riscv64")]
#[path = "riscv64/mod.rs"]
mod imp;

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

pub use imp::{
    // консоль (init приёма — внутри init_device_interrupts)
    console_drain, console_getc, console_has_input, console_init, console_take_lost, Console,
    CONSOLE_IRQ,
    // Веха 120: размер консоли в знакоместах (`(0, 0)` — арх его не знает: serial)
    console_size,
    // мышь (Веха 115): события с провода, позицию курсора ведёт владелец экрана
    mouse_pending, mouse_pop, mouse_present, mouse_take_lost, mouse_wheel,
    // клавиатура событиями (Веха 119): код клавиши + маска модификаторов + готовый ASCII
    key_pending, key_pop,
    // прерывания
    enable_interrupts, init_device_interrupts, irq_mask_preempt, irq_mask_read, irq_mask_stdin,
    irq_mask_idle, irq_mask_write, irq_restore, irq_save_disable, mark_in_kernel,
    wait_for_interrupt,
    // таймер
    now_cycles, now_ticks, timer_arm, timer_hw_init,
    // память
    clone_kernel_root, flush_tlb, free_address_space, map, mm_enable, mm_init, page_info,
    phys_to_virt, space_root, space_token, translate, unmap_shared, virt_to_phys, MAP_R,
    MAP_SHARED, MAP_U,
    MAP_W, MAP_WC,
    MAP_X,
    MM_NAME,
    // trap'ы и контексты
    context_switch, enter_user, trap_init, Context, TrapFrame,
    // устройства (Веха 27, virtio-net — Веха 34, AHCI — Веха 47, e1000 — Веха 49, IRQ — Веха 52)
    e1000_irq_setup, probe_ahci, probe_e1000, probe_virtio_blk, probe_virtio_net,
    // virtio-rng (долг Вехи 86, закрыт перед 95): аппаратная энтропия от гипервизора
    probe_virtio_rng,
    userdrv_irq_arm,
    // разное
    ARCH_NAME, ELF_MACHINE, USERSPACE_READY,
    // платформа (Веха 41/42): границы RAM из карты памяти + ранняя инициализация + детект металла
    is_real_hardware, platform_init, ram_total,
    // Веха 96: режим пиксельной консоли (x86 — фреймбуфер от GRUB; riscv — None)
    video_mode,
    // Веха 97: экран как ресурс — окно под capability, описание режима, передача владения
    video_give_to_user, video_info, video_owner, video_take_back, video_window,
    // часы и случайность (Веха 86): настенное время от прошивки (CMOS RTC / goldfish-rtc из DTB)
    // и аппаратный ГСЧ (RDRAND на x86; на riscv его нет — общий код мешает энтропию сам)
    hw_random_u64, wall_clock_unix_ns,
    // установщик на диск (Веха 48): загрузочный модуль multiboot2 (образ) — x86 отдаёт, riscv None
    boot_module,
};

/// Выключение машины — задел под автотесты (ядро само завершает QEMU); пока не зовётся.
#[allow(unused_imports)]
pub use imp::power_off;

/// Веха 50 — USB xHCI: поиск контроллера и байт клавиатуры в консоль. Только x86 (на riscv
/// USB нет, `xhci`-модуль ядра там — заглушка).
#[cfg(target_arch = "x86_64")]
pub use imp::{pci_dump, probe_xhci, usb_key};

/// Род page fault'а из U-mode — общий язык арха и `proc::handle_user_fault`
/// (ленивая куча обслуживает Load/Store; Exec в куче — гибель процесса, W^X).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    Load,
    Store,
    Exec,
}

impl FaultKind {
    /// Имя для диагностики («процесс убит»).
    pub fn name(self) -> &'static str {
        match self {
            FaultKind::Load => "load",
            FaultKind::Store => "store",
            FaultKind::Exec => "exec",
        }
    }
}

/// Классифицированный trap из U-mode: арх разбирает свой регистр причины (scause, вектор IDT…)
/// и отдаёт общему коду ([`crate::proc::handle_user_trap`]) уже смысл, а не номера.
#[derive(Clone, Copy)]
pub enum UserTrap {
    /// Системный вызов (ecall / syscall).
    Syscall,
    /// Page fault по адресу `va` (ленивая куча или гибель процесса).
    PageFault { va: usize, kind: FaultKind },
    /// Тик таймера — вытеснение процесса.
    TimerTick,
    /// Всё прочее — фатально для процесса; код причины в арх-кодировке (для печати).
    Unknown(usize),
}

/// Найденное virtio-blk устройство (Веха 27): где у него транспорт и каким номером
/// приходит его прерывание. Ищет арх ([`probe_virtio_blk`]) — у каждой архитектуры
/// своя шина (virtio-mmio на QEMU virt, virtio-pci на q35); говорит с устройством
/// общий драйвер [`crate::virtio_blk`] — virtqueue и рукопожатие статуса одинаковы.
pub struct BlkDevice {
    pub transport: BlkTransport,
    /// Номер прерывания в терминах арха: riscv — источник PLIC, x86 — вектор MSI-X.
    pub irq: u32,
}

/// Транспорт virtio-blk: адреса, по которым драйвер найдёт регистры устройства.
/// Все адреса — уже отображённая архом память (MMIO). Каждая архитектура конструирует
/// только СВОЙ вариант (mmio — riscv, pci — x86), но матчит драйвер оба — отсюда allow.
#[allow(dead_code)]
pub enum BlkTransport {
    /// virtio-mmio (QEMU virt): база слота регистров.
    Mmio { base: usize },
    /// virtio-pci modern (QEMU q35): MMIO-окна структур из vendor-capabilities;
    /// notify-адрес очереди q = `notify_base + queue_notify_off(q) * notify_mult`.
    Pci { common: usize, notify_base: usize, notify_mult: u32, isr: usize, device: usize },
}

/// Найденное virtio-net устройство (Веха 34). Транспорт — тот же split-virtqueue,
/// что у блока ([`BlkTransport`]: адреса регистров одинаковы для любого virtio),
/// но у сети ДВЕ очереди (0=приём, 1=передача) и нет IRQ: драйвер работает опросом
/// колец (прерывания, как синхронный путь blk на загрузке, отложены до потребности).
pub struct NetDevice {
    pub transport: BlkTransport,
    /// Веха 91 - прерывание ПРИЁМА в терминах арха (riscv - источник PLIC, x86 - вектор MSI-X);
    /// `0` - карта без прерывания, драйвер остаётся на опросе.
    pub irq: u32,
}
