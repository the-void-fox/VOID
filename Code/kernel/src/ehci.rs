//! Драйвер USB 2.0 — EHCI (Веха 199).
//!
//! ## Зачем он, когда есть xHCI
//!
//! xHCI (Веха 50) поднимает USB на машинах новее примерно 2012 года. На всём, что старше, —
//! включая ноутбук владельца — контроллер другой: **EHCI**, и без него у системы нет СВОЕЙ
//! клавиатуры. До этой вехи она работала там через эмуляцию прошивки (`Legacy USB Support` в
//! BIOS: прошивка ловит обращения к контроллеру 8042 и подставляет нажатия с USB). Костыль этот
//! ломается от любого шага в сторону — например, от перевода чипсета в режим ACPI, который нужен
//! кнопке питания (Веха 198.1): прошивка считает, что управление забрала система, и эмуляцию
//! выключает. Пока клавиатура держится на ней, включить кнопку питания нельзя.
//!
//! ## Чем EHCI отличается от всего, что мы писали раньше
//!
//! Тем, что он НЕ УМЕЕТ разговаривать с медленными устройствами. EHCI — контроллер только
//! высокой скорости (480 Мбит/с); клавиатуры и мыши работают на низкой (1,5) и полной (12), и
//! для них нужен **транслятор транзакций** — он живёт в USB-хабе. У Intel начиная с чипсетов
//! 5-й серии такой хаб встроен в сам чипсет, поэтому на живой машине картина всегда такая:
//!
//! ```text
//!   EHCI ──(высокая скорость)──> ХАБ ──(низкая/полная)──> клавиатура
//! ```
//!
//! Значит драйвер обязан уметь три вещи, которых не было ни у xHCI, ни у AHCI: отобрать
//! управление у прошивки, перечислить ХАБ и разговаривать через него **раздельными
//! транзакциями** (split). Ровно поэтому веха большая.
//!
//! ## Как устроено здесь
//!
//! Два списка, как велит спецификация: **асинхронный** (кольцо очередей для управляющих
//! передач) и **периодический** (таблица кадров для прерывающих — по ним приходят нажатия).
//! Всё — в обнулённых фреймах [`crate::frame`], физика = виртуальный адрес через прямую карту.
//! Опрос, без прерываний: как и у остальных наших драйверов, и по той же причине.
//!
//! ## Чего здесь нет
//!
//! Массовой памяти (флешка по EHCI — отдельная работа, bulk-передачи), нескольких хабов в
//! цепочке, изохронных передач (звук и камеры), управления питанием портов сверх включения.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};

use crate::frame;
use crate::sync::SpinLock;

// ─── регистры ────────────────────────────────────────────────────────────────
//
// Первым идёт блок возможностей (его длина — в первом байте), за ним операционный.

const CAP_HCSPARAMS: usize = 0x04; // число портов [3:0]
const CAP_HCCPARAMS: usize = 0x08; // EECP [15:8] — где в конфиге PCI лежит отъём у прошивки

const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_USBINTR: usize = 0x08;
const OP_FRINDEX: usize = 0x0c;
const OP_CTRLDSSEGMENT: usize = 0x10;
const OP_PERIODICLIST: usize = 0x14;
const OP_ASYNCLIST: usize = 0x18;
const OP_CONFIGFLAG: usize = 0x40;
const OP_PORTSC: usize = 0x44;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_PERIODIC_EN: u32 = 1 << 4;
const CMD_ASYNC_EN: u32 = 1 << 5;
const STS_HALTED: u32 = 1 << 12;

const PORT_CONNECT: u32 = 1 << 0;
const PORT_ENABLE: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 8;
const PORT_POWER: u32 = 1 << 12;
/// Биты-изменения порта (RW1C): снимаются записью единицы, и трогать их случайно нельзя.
const PORT_CHANGES: u32 = (1 << 1) | (1 << 3) | (1 << 5);

/// Скорость устройства в терминах EHCI (поле EPS дескриптора очереди).
const EPS_FULL: u32 = 0;
const EPS_LOW: u32 = 1;
const EPS_HIGH: u32 = 2;

/// Признак конца списка (бит T в указателях).
const LINK_TERM: u32 = 1;
/// Тип элемента в указателе списка: очередь (QH).
const LINK_QH: u32 = 1 << 1;

/// Код направления в токене передачи.
const PID_OUT: u32 = 0;
const PID_IN: u32 = 1;
const PID_SETUP: u32 = 2;
/// Передача ещё выполняется (бит Active в статусе токена).
const TD_ACTIVE: u32 = 1 << 7;
/// Ошибки, о которых стоит говорить: остановка, битый пакет, переполнение.
const TD_ERRORS: u32 = 0x78;
/// Сказать контроллеру «сообщи о завершении» (бит IOC). Прерываний мы не просим, но бит
/// заодно означает «этот дескриптор — последний в цепочке», и по нему удобно читать статус.
const TD_IOC: u32 = 1 << 15;

/// Кадров в периодической таблице. Тысяча двадцать четыре — единственный размер, который
/// обязан поддерживать любой контроллер.
const FRAMES: usize = 1024;

/// Устройство на шине: адрес, скорость и через какой порт какого хаба оно подключено.
///
/// Последнее — не украшение: для низко- и полноскоростного устройства контроллер обязан знать,
/// КОМУ адресовать раздельную транзакцию, и адрес хаба с номером порта едут в каждой очереди.
#[derive(Clone, Copy, Default)]
struct Dev {
    addr: u8,
    speed: u32,
    hub_addr: u8,
    hub_port: u8,
}

pub struct Ehci {
    op: usize,        // база операционных регистров
    ports: u32,       // сколько портов у корневого концентратора
    qh: usize,        // очередь асинхронного списка (голова кольца)
    td: usize,        // фрейм под цепочку дескрипторов передачи
    buf: usize,       // фрейм под данные передач
    frames: usize,    // периодическая таблица кадров
    int_qh: usize,    // очередь прерывающей передачи (клавиатура)
    int_td: usize,    // её дескриптор
    int_buf: usize,   // её буфер (восьмибайтные отчёты клавиатуры)
    next_addr: u8,    // какой адрес выдадим следующему устройству
    kbd: Option<Dev>, // клавиатура, если нашлась
    kbd_toggle: u32,  // бит переключения данных прерывающей передачи
    prev: [u8; 6],    // прошлый набор нажатых клавиш
}
unsafe impl Send for Ehci {}

static EHCI: SpinLock<Option<Ehci>> = SpinLock::new(None);

#[inline]
unsafe fn rd(a: usize) -> u32 {
    read_volatile(a as *const u32)
}
#[inline]
unsafe fn wr(a: usize, v: u32) {
    write_volatile(a as *mut u32, v);
}
/// Указатель на структуру в памяти контроллера (физический адрес → наш).
#[inline]
fn dm(pa: usize) -> *mut u32 {
    frame::ptr(pa) as *mut u32
}

/// Отобрать управление у прошивки (EHCI Extended Capabilities: USBLEGSUP).
///
/// До этого момента контроллером владеет BIOS — именно он эмулирует USB-клавиатуру через
/// контроллер 8042. Мы просим владение битом «OS Owned» и ЖДЁМ, пока прошивка снимет свой бит.
/// Пропустить этот шаг нельзя: два владельца у одного контроллера — это состязание за регистры,
/// в котором проигрывают оба.
unsafe fn bios_handoff(base: usize, bdf: u16) -> bool {
    let Some(eecp) = legsup_offset(base, bdf) else { return false };
    let legsup = crate::arch::pci_cfg_read32(bdf, eecp);
    if legsup & BIOS_OWNED == 0 {
        return false; // прошивка и не владела — возвращать потом будет нечего
    }
    crate::arch::pci_cfg_write32(bdf, eecp, legsup | OS_OWNED);
    for _ in 0..100_000 {
        if crate::arch::pci_cfg_read32(bdf, eecp) & BIOS_OWNED == 0 {
            crate::println!("  [usb]  EHCI: управление отобрано у прошивки");
            return true;
        }
        core::hint::spin_loop();
    }
    // Прошивка не отпустила. Сказать вслух и продолжить: чаще всего она уже не работает с
    // контроллером, а молча идти дальше значило бы прятать причину будущих странностей.
    crate::println!("  [usb]  EHCI: прошивка не отдала управление — работаем всё равно");
    true
}

/// Где в конфигурации PCI лежит «кому принадлежит контроллер» (USB Legacy Support).
///
/// Номер возможности проверяем (он обязан быть 1): расширенные возможности EHCI — это список, и
/// записать «я владелец» не в ту из них значит наугад править чужой регистр устройства.
fn legsup_offset(base: usize, bdf: u16) -> Option<u8> {
    let eecp = unsafe { (rd(base + CAP_HCCPARAMS) >> 8) & 0xff };
    if eecp < 0x40 {
        return None;
    }
    let cap = crate::arch::pci_cfg_read32(bdf, eecp as u8);
    (cap & 0xff == 1).then_some(eecp as u8)
}

const OS_OWNED: u32 = 1 << 24;
const BIOS_OWNED: u32 = 1 << 16;

/// ВЕРНУТЬ контроллер прошивке (Веха 199).
///
/// Нужно ровно в одном случае, и он важнее, чем кажется: мы отобрали управление, а СВОЕЙ
/// клавиатуры на контроллере не нашли. Пока управление у прошивки, она эмулирует USB-клавиатуру
/// через контроллер 8042 — именно так работает клавиатура на машинах, где нашего драйвера не
/// хватает. Забрать контроллер и не дать взамен ничего значит оставить человека без ввода.
///
/// Поэтому: остановить контроллер, отдать порты спутникам (CONFIGFLAG = 0) и снять свой бит
/// владения. Прошивка увидит это и продолжит эмулировать, как будто нас не было.
unsafe fn release_to_bios(base: usize, op: usize, bdf: u16) {
    wr(op + OP_USBCMD, rd(op + OP_USBCMD) & !(CMD_RUN | CMD_ASYNC_EN | CMD_PERIODIC_EN));
    for _ in 0..1_000_000 {
        if rd(op + OP_USBSTS) & STS_HALTED != 0 {
            break;
        }
    }
    wr(op + OP_CONFIGFLAG, 0);
    if let Some(eecp) = legsup_offset(base, bdf) {
        let legsup = crate::arch::pci_cfg_read32(bdf, eecp);
        crate::arch::pci_cfg_write32(bdf, eecp, legsup & !OS_OWNED);
    }
    crate::println!("  [usb]  EHCI: своей клавиатуры нет — управление возвращено прошивке");
}

/// Подождать `ms` миллисекунд по измеренной таймбазе.
///
/// Веха 199.1 — без НАСТОЯЩЕЙ задержки драйвер не работает на живом железе, и это первое, что
/// показала машина владельца. Спецификация USB требует после подачи питания на порт выдержку
/// в 100 мс: раньше этого срока порт честно отвечает «никого нет». В эмуляторе устройство
/// подключено мгновенно, поэтому цикл «покрутиться сто тысяч раз» проходил — а на ноутбуке
/// означал «спросить и уйти», и оба контроллера выглядели пустыми.
fn wait_ms(ms: u64) {
    let until = crate::clock::uptime_ns() + ms * 1_000_000;
    while crate::clock::uptime_ns() < until {
        core::hint::spin_loop();
    }
}

/// Поднять контроллеры USB 2.0. `false` — их на машине нет либо своей клавиатуры на них не
/// нашлось (тогда управление возвращено прошивке).
pub fn init() -> bool {
    // Контроллеров бывает несколько, и клавиатура может быть на любом из них.
    let mut found = [(0usize, 0u16); 4];
    let n = crate::arch::probe_ehci(&mut found);
    for &(base, bdf) in found.iter().take(n) {
        if init_one(base, bdf) {
            return true;
        }
    }
    false
}

/// Поднять ОДИН контроллер. `true` — на нём нашлась своя клавиатура.
fn init_one(base: usize, bdf: u16) -> bool {
    unsafe {
        let took = bios_handoff(base, bdf);
        let caplen = (read_volatile(base as *const u8)) as usize;
        let op = base + caplen;
        let ports = rd(base + CAP_HCSPARAMS) & 0xf;

        // Остановить и сбросить: прошивка могла оставить контроллер в любом состоянии.
        wr(op + OP_USBCMD, rd(op + OP_USBCMD) & !CMD_RUN);
        for _ in 0..1_000_000 {
            if rd(op + OP_USBSTS) & STS_HALTED != 0 {
                break;
            }
        }
        wr(op + OP_USBCMD, CMD_RESET);
        for _ in 0..1_000_000 {
            if rd(op + OP_USBCMD) & CMD_RESET == 0 {
                break;
            }
        }

        let (Some(qh), Some(td), Some(buf)) = (frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return false;
        };
        let (Some(frames), Some(int_qh), Some(int_td), Some(int_buf)) =
            (frame::alloc(), frame::alloc(), frame::alloc(), frame::alloc())
        else {
            return false;
        };

        // Пустая периодическая таблица: все кадры «ничего не делать». Наполнится, когда
        // появится прерывающая передача (клавиатура).
        for i in 0..FRAMES {
            write_volatile(dm(frames).add(i), LINK_TERM);
        }

        // Голова асинхронного кольца: очередь, ссылающаяся сама на себя. Бит H говорит
        // контроллеру, что с неё начинается обход, — без него он не знает, где круг.
        write_volatile(dm(qh), qh as u32 | LINK_QH);
        write_volatile(dm(qh).add(1), 1 << 15); // H = 1, адрес 0, эндпоинт 0
        write_volatile(dm(qh).add(2), 0);
        write_volatile(dm(qh).add(3), 0);
        write_volatile(dm(qh).add(4), LINK_TERM); // следующий дескриптор: нет
        write_volatile(dm(qh).add(5), LINK_TERM);
        write_volatile(dm(qh).add(6), 0);

        let mut e = Ehci {
            op, ports, qh, td, buf, frames,
            int_qh, int_td, int_buf,
            next_addr: 1,
            kbd: None,
            kbd_toggle: 0,
            prev: [0; 6],
        };

        wr(op + OP_USBINTR, 0); // прерываний не просим: опрос
        wr(op + OP_CTRLDSSEGMENT, 0); // все структуры ниже 4 ГиБ
        wr(op + OP_FRINDEX, 0);
        wr(op + OP_PERIODICLIST, frames as u32);
        wr(op + OP_ASYNCLIST, qh as u32);
        wr(op + OP_USBCMD, CMD_RUN | CMD_ASYNC_EN | CMD_PERIODIC_EN | (8 << 16));
        wr(op + OP_CONFIGFLAG, 1); // порты — нам, а не спутникам (UHCI/OHCI)
        for _ in 0..1_000_000 {
            if rd(op + OP_USBSTS) & STS_HALTED == 0 {
                break;
            }
        }

        // Выдержка после подачи питания: спецификация требует 100 мс, берём с запасом. Без
        // неё порты отвечают «никого нет» — см. `wait_ms`.
        wait_ms(150);
        crate::println!("  [usb]  EHCI: контроллер {:#x} поднят, портов {}", base, ports);
        let found = e.enumerate_root();
        // Клавиатуры не нашлось — вернуть контроллер прошивке. Пока он у неё, она эмулирует
        // USB-клавиатуру через 8042, и на машине, где наш драйвер не справился, это
        // единственный способ ввода. Забрать и не дать взамен — худшее из возможного.
        if e.kbd.is_none() && took {
            release_to_bios(base, op, bdf);
            return false;
        }
        *EHCI.lock_irq() = Some(e);
        found
    }
}

impl Ehci {
    /// Пройти корневые порты: включить питание, сбросить порт, перечислить то, что нашлось.
    unsafe fn enumerate_root(&mut self) -> bool {
        let mut any = false;
        for p in 0..self.ports {
            let psc = self.op + OP_PORTSC + p as usize * 4;
            let v = rd(psc);
            if v & PORT_POWER == 0 {
                wr(psc, (v & !PORT_CHANGES) | PORT_POWER);
                wait_ms(120); // питание подано — ждём, пока порт определится
            }
            if rd(psc) & PORT_CONNECT == 0 {
                crate::println!("  [usb]  EHCI: порт {} пуст ({:#010x})", p + 1, rd(psc));
                continue;
            }
            crate::println!("  [usb]  EHCI: на порту {} что-то есть — сбрасываю", p + 1);
            if !self.reset_port(psc) {
                // Порт не включился: устройство не высокоскоростное, и контроллер отдал его
                // спутнику (UHCI/OHCI). На машинах со встроенным хабом так не бывает, а на
                // старых — бывает, и сказать об этом честнее, чем молчать.
                crate::println!("  [usb]  EHCI: порт {} отдан спутнику (не высокая скорость)", p + 1);
                continue;
            }
            any |= self.enumerate_device(Dev { addr: 0, speed: EPS_HIGH, hub_addr: 0, hub_port: 0 });
        }
        any
    }

    /// Сбросить порт и дождаться включения. `false` — устройство не высокоскоростное.
    unsafe fn reset_port(&mut self, psc: usize) -> bool {
        let v = rd(psc) & !PORT_CHANGES & !PORT_ENABLE;
        wr(psc, v | PORT_RESET);
        wait_ms(50); // спецификация: держать сброс не меньше 50 мс
        wr(psc, rd(psc) & !PORT_CHANGES & !PORT_RESET);
        for _ in 0..1_000_000 {
            if rd(psc) & PORT_RESET == 0 {
                break;
            }
        }
        rd(psc) & PORT_ENABLE != 0
    }

    /// Собрать очередь под устройство: адрес, скорость, размер пакета и — для медленных —
    /// адрес хаба с портом, через которые контроллер сделает раздельную транзакцию.
    unsafe fn setup_qh(&mut self, dev: &Dev, ep: u32, mps: u32, control: bool) {
        let mut chars = (dev.addr as u32)
            | (ep << 8)
            | (dev.speed << 12)
            | (1 << 14) // DTC: бит переключения берём из дескриптора передачи
            | (1 << 15) // H: это голова кольца
            | (mps << 16)
            | (3 << 28); // NakCnt
        if control && dev.speed != EPS_HIGH {
            chars |= 1 << 27; // C: управляющий эндпоинт медленного устройства
        }
        write_volatile(dm(self.qh).add(1), chars);
        write_volatile(
            dm(self.qh).add(2),
            (1 << 30) // Mult = 1 транзакция за микрокадр
                | ((dev.hub_addr as u32) << 16)
                | ((dev.hub_port as u32) << 23),
        );
        write_volatile(dm(self.qh).add(3), 0);
        write_volatile(dm(self.qh).add(4), LINK_TERM);
        write_volatile(dm(self.qh).add(5), LINK_TERM);
        write_volatile(dm(self.qh).add(6), 0); // токен: не активен
    }

    /// Записать один дескриптор передачи по индексу в общем фрейме.
    ///
    /// `next` — индекс следующего (или `None`, если этот последний).
    unsafe fn write_td(&mut self, i: usize, next: Option<usize>, pid: u32, toggle: u32,
                       buf: usize, len: u32, ioc: bool) {
        let td = dm(self.td).add(i * 8);
        write_volatile(td, match next {
            Some(n) => (self.td + n * 32) as u32,
            None => LINK_TERM,
        });
        write_volatile(td.add(1), LINK_TERM); // альтернативного пути нет
        let token = TD_ACTIVE
            | (pid << 8)
            | (3 << 10) // три попытки при ошибке
            | (len << 16)
            | (toggle << 31)
            | if ioc { TD_IOC } else { 0 };
        write_volatile(td.add(2), token);
        // Буфер: EHCI адресует страницами, но наши передачи короче страницы — хватает одной.
        write_volatile(td.add(3), buf as u32);
        for k in 1..5 {
            write_volatile(td.add(3 + k), 0);
        }
    }

    /// Запустить цепочку дескрипторов с индекса 0 и дождаться конца. Возвращает статус
    /// последнего дескриптора (`None` — не дождались).
    unsafe fn run_chain(&mut self, last: usize) -> Option<u32> {
        compiler_fence(Ordering::SeqCst);
        write_volatile(dm(self.qh).add(4), self.td as u32); // следующий дескриптор очереди
        write_volatile(dm(self.qh).add(6), 0); // токен очереди: пусть возьмёт из дескриптора
        compiler_fence(Ordering::SeqCst);
        let td = dm(self.td).add(last * 8);
        for _ in 0..20_000_000 {
            let token = read_volatile(td.add(2));
            if token & TD_ACTIVE == 0 {
                return Some(token);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Управляющая передача. `data` — что прочитать (IN) в общий буфер; `len` — сколько.
    /// `true` — устройство ответило без ошибок.
    unsafe fn control(&mut self, dev: &Dev, req_type: u8, request: u8, value: u16, index: u16,
                      len: u16, mps: u32) -> bool {
        self.setup_qh(dev, 0, mps, true);
        // Пакет запроса — в начале общего буфера, данные следом.
        let setup = frame::ptr(self.buf) as *mut u8;
        write_volatile(setup, req_type);
        write_volatile(setup.add(1), request);
        write_volatile(setup.add(2), value as u8);
        write_volatile(setup.add(3), (value >> 8) as u8);
        write_volatile(setup.add(4), index as u8);
        write_volatile(setup.add(5), (index >> 8) as u8);
        write_volatile(setup.add(6), len as u8);
        write_volatile(setup.add(7), (len >> 8) as u8);
        let data = self.buf + 64;
        let last = if len > 0 {
            self.write_td(0, Some(1), PID_SETUP, 0, self.buf, 8, false);
            let dir_in = req_type & 0x80 != 0;
            self.write_td(1, Some(2), if dir_in { PID_IN } else { PID_OUT }, 1, data, len as u32, false);
            // Статус идёт в обратную сторону и всегда с переключателем 1.
            self.write_td(2, None, if dir_in { PID_OUT } else { PID_IN }, 1, 0, 0, true);
            2
        } else {
            self.write_td(0, Some(1), PID_SETUP, 0, self.buf, 8, false);
            self.write_td(1, None, PID_IN, 1, 0, 0, true);
            1
        };
        match self.run_chain(last) {
            Some(token) => token & TD_ERRORS == 0,
            None => false,
        }
    }

    /// Прочитать дескриптор устройства (или другой) в общий буфер и скопировать наружу.
    unsafe fn get_descriptor(&mut self, dev: &Dev, dtype: u8, index: u8, out: &mut [u8], mps: u32) -> bool {
        let value = (dtype as u16) << 8 | index as u16;
        if !self.control(dev, 0x80, 6, value, 0, out.len() as u16, mps) {
            return false;
        }
        core::ptr::copy_nonoverlapping(
            frame::ptr(self.buf + 64) as *const u8,
            out.as_mut_ptr(),
            out.len(),
        );
        true
    }

    /// Перечислить устройство на уже сброшенном порту: выдать адрес, прочитать дескрипторы и,
    /// если это хаб или клавиатура, заняться им по-своему.
    unsafe fn enumerate_device(&mut self, mut dev: Dev) -> bool {
        // Первые восемь байт дескриптора читаются с размером пакета 8: настоящий размер как раз
        // в них и лежит, а спрашивать больше, чем устройство умеет принять, нельзя.
        // ── Веха 199.24 — КАЖДЫЙ ОТКАЗ НАЗЫВАЕТ СЕБЯ ──
        //
        // Все четыре выхода отсюда были молчаливыми `return false`, и на машине владельца это
        // выглядело так: «на порту 1 что-то есть — сбрасываю», и дальше НИ СЛОВА. Устройство
        // на порту есть (у чипсетов Intel 5-series это встроенный rate-matching hub, он всегда
        // на первом порту), перечисление срывается — а на каком из четырёх шагов, не видно.
        //
        // Шагов ровно четыре, и каждый значит своё: не ответило на первый запрос (нет обмена
        // вообще), не приняло адрес, не отдало полный дескриптор, не приняло конфигурацию.
        // Разница между ними — это разница между «раздельные транзакции не работают» и
        // «устройство не то, что мы думали».
        let mut head = [0u8; 8];
        if !self.get_descriptor(&dev, 1, 0, &mut head, 8) {
            crate::println!("  [usb]  EHCI: устройство не ответило на первый запрос дескриптора");
            return false;
        }
        let mps = head[7] as u32;
        // Выдать адрес. С этого момента устройство слушает только его.
        let addr = self.next_addr;
        if !self.control(&dev, 0x00, 5, addr as u16, 0, 0, mps.max(8)) {
            crate::println!("  [usb]  EHCI: устройство не приняло адрес {}", addr);
            return false;
        }
        self.next_addr += 1;
        dev.addr = addr;
        wait_ms(5); // устройству дают время принять адрес (спецификация: 2 мс)

        let mut desc = [0u8; 18];
        if !self.get_descriptor(&dev, 1, 0, &mut desc, mps) {
            crate::println!(
                "  [usb]  EHCI: адрес {} выдан, но полный дескриптор не читается (пакет {})",
                addr, mps,
            );
            return false;
        }
        let vid = desc[8] as u16 | (desc[9] as u16) << 8;
        let pid = desc[10] as u16 | (desc[11] as u16) << 8;
        let class = desc[4];
        if !self.control(&dev, 0x00, 9, 1, 0, 0, mps) {
            crate::println!(
                "  [usb]  EHCI: {:04x}:{:04x} не приняло конфигурацию (класс {})",
                vid, pid, class,
            );
            return false; // SET_CONFIGURATION(1)
        }
        if class == 9 {
            crate::println!("  [usb]  EHCI: хаб {:04x}:{:04x} (адрес {})", vid, pid, addr);
            return self.enumerate_hub(&dev, mps);
        }
        crate::println!(
            "  [usb]  EHCI: устройство {:04x}:{:04x} класс {} (адрес {})",
            vid, pid, class, addr,
        );
        self.setup_keyboard(&dev, mps)
    }

    /// Хаб: включить порты, сбросить занятые и перечислить то, что за ними.
    unsafe fn enumerate_hub(&mut self, hub: &Dev, mps: u32) -> bool {
        let mut hd = [0u8; 8];
        if !self.get_descriptor(hub, 0x29, 0, &mut hd, mps) {
            return false;
        }
        let nports = hd[2];
        crate::println!("  [usb]  EHCI: у хаба портов {}", nports);
        let mut any = false;
        for port in 1..=nports {
            // Включить питание порта: SET_FEATURE(PORT_POWER=8).
            self.control(hub, 0x23, 3, 8, port as u16, 0, mps);
        }
        wait_ms(120); // дать портам хаба подняться после подачи питания
        for port in 1..=nports {
            let mut st = [0u8; 4];
            if !self.control(hub, 0xa3, 0, 0, port as u16, 4, mps) {
                continue;
            }
            core::ptr::copy_nonoverlapping(frame::ptr(self.buf + 64) as *const u8, st.as_mut_ptr(), 4);
            let status = st[0] as u16 | (st[1] as u16) << 8;
            if status & 1 == 0 {
                continue; // на порту никого
            }
            // Сброс порта: SET_FEATURE(PORT_RESET=4), затем ждём и читаем состояние снова.
            self.control(hub, 0x23, 3, 4, port as u16, 0, mps);
            wait_ms(60); // сброс порта хаба: те же 50 мс, что у корневого, с запасом
            if !self.control(hub, 0xa3, 0, 0, port as u16, 4, mps) {
                continue;
            }
            core::ptr::copy_nonoverlapping(frame::ptr(self.buf + 64) as *const u8, st.as_mut_ptr(), 4);
            let status = st[0] as u16 | (st[1] as u16) << 8;
            if status & 2 == 0 {
                crate::println!("  [usb]  EHCI: порт {} хаба не включился (состояние {:#06x})", port, status);
                continue;
            }
            // Скорость: бит 9 — низкая, бит 10 — высокая, иначе полная.
            let speed = if status & (1 << 9) != 0 {
                EPS_LOW
            } else if status & (1 << 10) != 0 {
                EPS_HIGH
            } else {
                EPS_FULL
            };
            // Для медленного устройства транслятором служит ЭТОТ хаб: его адрес и номер порта
            // едут в каждой очереди, иначе контроллер не знает, кому адресовать раздельную
            // транзакцию.
            let (hub_addr, hub_port) = if speed == EPS_HIGH {
                (0, 0)
            } else {
                (hub.addr, port)
            };
            any |= self.enumerate_device(Dev { addr: 0, speed, hub_addr, hub_port });
        }
        any
    }

    /// Если это HID-клавиатура — настроить её прерывающий эндпоинт и поставить первый запрос.
    unsafe fn setup_keyboard(&mut self, dev: &Dev, mps: u32) -> bool {
        let mut cfg = [0u8; 64];
        if !self.get_descriptor(dev, 2, 0, &mut cfg, mps) {
            return false;
        }
        let (mut iface, mut is_hid, mut ep_addr, mut ep_mps) = (0u8, false, 0u8, 8u32);
        let mut i = cfg[0] as usize;
        while i + 4 <= cfg.len() && cfg[i] != 0 {
            let (blen, btype) = (cfg[i] as usize, cfg[i + 1]);
            if btype == 4 {
                iface = cfg[i + 2];
                // Класс 3 (HID), протокол 1 (клавиатура): мышь по USB мы пока не ведём.
                is_hid = cfg[i + 5] == 3 && cfg[i + 7] == 1;
            } else if btype == 5 && is_hid && cfg[i + 2] & 0x80 != 0 && cfg[i + 3] & 3 == 3 {
                ep_addr = cfg[i + 2];
                ep_mps = cfg[i + 4] as u32 | (cfg[i + 5] as u32) << 8;
            }
            i += blen.max(1);
        }
        if ep_addr == 0 {
            return false;
        }
        // Загрузочный протокол: клавиатура шлёт восьмибайтные отчёты известного вида.
        self.control(dev, 0x21, 0x0b, 0, iface as u16, 0, mps);
        self.setup_interrupt(dev, (ep_addr & 0x0f) as u32, ep_mps);
        self.kbd = Some(*dev);
        crate::println!("  [usb]  EHCI: HID-клавиатура готова (адрес {})", dev.addr);
        true
    }

    /// Прерывающая передача: очередь в периодической таблице + первый дескриптор.
    unsafe fn setup_interrupt(&mut self, dev: &Dev, ep: u32, mps: u32) {
        let qh = self.int_qh;
        write_volatile(dm(qh), LINK_TERM);
        write_volatile(
            dm(qh).add(1),
            (dev.addr as u32) | (ep << 8) | (dev.speed << 12) | (mps << 16) | (3 << 28),
        );
        // S-mask: в каком микрокадре начинать (первый), C-mask: где забирать ответ у хаба —
        // для медленного устройства обязателен, иначе раздельная транзакция не завершится.
        let cmask = if dev.speed == EPS_HIGH { 0 } else { 0x1c << 8 };
        write_volatile(
            dm(qh).add(2),
            0x01 | cmask | (1 << 30) | ((dev.hub_addr as u32) << 16) | ((dev.hub_port as u32) << 23),
        );
        write_volatile(dm(qh).add(3), 0);
        write_volatile(dm(qh).add(4), LINK_TERM);
        write_volatile(dm(qh).add(5), LINK_TERM);
        write_volatile(dm(qh).add(6), 0);
        // Все кадры таблицы указывают на эту очередь: опрашивать чаще, чем раз в кадр, нам
        // незачем, а реже — значит терять нажатия.
        for i in 0..FRAMES {
            write_volatile(dm(self.frames).add(i), qh as u32 | LINK_QH);
        }
        self.kbd_toggle = 0;
        self.queue_report();
    }

    /// Поставить запрос очередного отчёта клавиатуры.
    unsafe fn queue_report(&mut self) {
        let td = dm(self.int_td);
        write_volatile(td, LINK_TERM);
        write_volatile(td.add(1), LINK_TERM);
        write_volatile(
            td.add(2),
            TD_ACTIVE | (PID_IN << 8) | (3 << 10) | (8 << 16) | (self.kbd_toggle << 31) | TD_IOC,
        );
        write_volatile(td.add(3), self.int_buf as u32);
        for k in 1..5 {
            write_volatile(td.add(3 + k), 0);
        }
        compiler_fence(Ordering::SeqCst);
        write_volatile(dm(self.int_qh).add(4), self.int_td as u32);
        write_volatile(dm(self.int_qh).add(6), 0);
    }

    /// Опрос: пришёл ли отчёт клавиатуры, и если да — отдать нажатия наружу.
    unsafe fn poll_keyboard(&mut self) {
        if self.kbd.is_none() {
            return;
        }
        let token = read_volatile(dm(self.int_td).add(2));
        if token & TD_ACTIVE != 0 {
            return; // ещё выполняется
        }
        if token & TD_ERRORS == 0 {
            let r = core::slice::from_raw_parts(frame::ptr(self.int_buf), 8);
            crate::usb_hid::report(&mut self.prev, r);
            self.kbd_toggle ^= 1;
        }
        self.queue_report();
    }
}

/// Опрос клавиатуры — зовётся из общего пути консоли, как и у xHCI.
pub fn poll() {
    if let Some(e) = EHCI.lock_irq().as_mut() {
        unsafe { e.poll_keyboard() }
    }
}

/// Есть ли у нас клавиатура на EHCI (нужно тому же, кому и у xHCI: без прерываний её опрашивает
/// таймер, и гасить его в простое нельзя).
pub fn has_keyboard() -> bool {
    EHCI.lock_irq().as_ref().is_some_and(|e| e.kbd.is_some())
}
