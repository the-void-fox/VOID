//! Протокол окон (Веха 117, [[0007-graphics-native-compositor]], [[0016-void-ui-toolkit]]).
//!
//! ## Устройство в одном абзаце
//!
//! Композитор — обычный IPC-сервер, как файловый или сетевой. Клиент получает право звать его
//! (тем же способом, что stdio — [[process-stdio]]), просит окно, рисует пиксели В СВОЮ ПАМЯТЬ,
//! кладёт их **объектом в store** и говорит «готово», назвав content-id. Композитор читает
//! объект и складывает кадр. Ядро про окна не знает ничего.
//!
//! ## Почему буфер — объект, а не адрес
//!
//! Решено в [[0018-gpu-ladder]] и это главное, что делает протокол долговечным: пиксели
//! адресуются СОДЕРЖИМЫМ, а не местом. Отсюда сразу три следствия.
//!
//! 1. **Переезд на GPU не меняет протокол.** Сегодня объект лежит в RAM, завтра — в памяти
//!    видеокарты; клиент говорит то же самое. Отдавай клиент указатель, первый же шаг к железу
//!    переписывал бы всех клиентов разом.
//! 2. **Одинаковое содержимое — один объект.** Store дедуплицирует: окно, которое не менялось,
//!    не стоит ни байта при повторном `commit`, а композитор по content-id видит, что перечитывать
//!    нечего.
//! 3. **Окна могут пережить перезагрузку** — то, ради чего вся модель и затевалась (ADR 0007):
//!    содержимое окна уже персистентно, остаётся сохранить их список.
//!
//! Честная цена: каждый ПЕРЕРИСОВАННЫЙ кадр проходит через хэширование и копию. Пока окна
//! статичны (а при анимации перемещения они статичны — двигает их композитор), это ничего не
//! стоит. Настоящая цена появится у видео и игр — там понадобится разделяемый буфер, и это
//! записано долгом, а не забыто.
//!
//! ## Кадр доставляет композитор
//!
//! Клиент говорит «готово» и объявляет ПРЯМОУГОЛЬНИК изменений; куда и когда это попадёт на
//! экран, решает композитор. Так же устроены анимации (ADR 0016): их часы — у него, потому что
//! двигать окно, ничего не перерисовывая, может только он.

/// Создать окно: `[ширина u16, высота u16, заголовок…]` → `[id u32]`.
pub const OP_CREATE: usize = 1;
/// Привязать пиксели ПРЯМОУГОЛЬНИКА: `[id u32, x u16, y u16, w u16, h u16, content-id 32Б]`.
/// Объект содержит ровно `w*h*4` байта RGBA8888 по строкам — только эту область, а не всё окно.
///
/// Почему прямоугольником, а не окном целиком (Веха 118): полный кадр терминала 900×520 — это
/// 1,87 МБ, и КАЖДОЕ нажатие клавиши создавало бы новый объект такого размера. Куча ядра
/// кончалась за восемь нажатий — паникой. Полоса из пары текстовых строк в двадцать раз меньше,
/// и это единственное, что на самом деле изменилось.
pub const OP_ATTACH: usize = 2;
/// Кадр готов: `[id u32, x u16, y u16, w u16, h u16]` — прямоугольник изменений.
pub const OP_COMMIT: usize = 3;
/// Ждать событие: `[id u32]` → ответ ОТЛОЖЕННЫЙ, как чтение stdin у терминала.
pub const OP_EVENT: usize = 4;
/// Закрыть окно: `[id u32]`.
pub const OP_DESTROY: usize = 5;
/// Забрать событие БЕЗ ожидания: `[id u32]` → событие или пустой ответ.
///
/// Нужен клиентам, у которых есть свой реактор: терминал — сам сервер для своих панелей, и
/// уснуть в нашем вызове он не может, иначе перестанет обслуживать детей. Простым программам
/// (`winbox`) по-прежнему проще спать в [`OP_EVENT`].
pub const OP_POLL: usize = 6;

/// Событие: движение мыши над окном — `[1, x u16, y u16]` (координаты внутри окна).
pub const EV_MOTION: u8 = 1;
/// Кнопка мыши — `[2, x u16, y u16, кнопки u8, нажата u8]`.
pub const EV_BUTTON: u8 = 2;
/// Клавиша — `[3, байт]`.
pub const EV_KEY: u8 = 3;
/// Окно просят закрыться — `[4]`.
pub const EV_CLOSE: u8 = 4;

/// Имя переменной окружения, в которой хост объявляет индекс права на себя. То же соглашение,
/// что у stdio: искать эндпоинт перебором нельзя — «проба» это отправка сообщения, то есть
/// побочный эффект.
pub const ENV: &str = "WM";

/// Событие, разобранное клиентом.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Motion { x: u16, y: u16 },
    Button { x: u16, y: u16, buttons: u8, down: bool },
    Key(u8),
    Close,
}

/// Прочитать u16 из ответа (little-endian), не выходя за его край.
fn rd16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

impl Event {
    /// Разобрать байты ответа. `None` — пустой или непонятный ответ.
    pub fn parse(b: &[u8]) -> Option<Event> {
        match *b.first()? {
            EV_MOTION => Some(Event::Motion { x: rd16(b, 1)?, y: rd16(b, 3)? }),
            EV_BUTTON => Some(Event::Button {
                x: rd16(b, 1)?,
                y: rd16(b, 3)?,
                buttons: *b.get(5)?,
                down: *b.get(6)? != 0,
            }),
            EV_KEY => Some(Event::Key(*b.get(1)?)),
            EV_CLOSE => Some(Event::Close),
            _ => None,
        }
    }
}

// ── клиентская сторона ───────────────────────────────────────────────────────
//
// Тонкая обёртка над `SYS_CALL`: у приложения не должно быть повода знать раскладку байтов.
// Кучи здесь НЕТ намеренно: библиотеку линкуют и программы без аллокатора, а сообщения
// протокола и так помещаются в десятки байт.

/// Потолок длины заголовка окна. Больше на титульной полосе всё равно не поместится.
pub const TITLE_MAX: usize = 60;

/// Право звать композитор (`WM=<индекс>` в окружении). `None` — окон в этой сессии нет.
///
/// Разбор такой же, как у stdio, и это не совпадение: оба — ХОСТЫ, которых процессу дал
/// родитель, а не права, выданные системой на старте. У вторых имя начинается с `CAP_`
/// (`CAP_STORE`, `CAP_FB` — их кладёт init), у первых — нет.
pub fn endpoint() -> Option<usize> {
    let mut buf = [0u8; 512];
    let n = crate::env(&mut buf).min(buf.len());
    let mut key = [0u8; 16];
    key[..ENV.len()].copy_from_slice(ENV.as_bytes());
    key[ENV.len()] = b'=';
    let klen = ENV.len() + 1;
    for entry in buf[..n].split(|&b| b == 0) {
        if entry.len() <= klen || entry[..klen] != key[..klen] {
            continue;
        }
        let mut idx = 0usize;
        let mut any = false;
        for &d in &entry[klen..] {
            if !d.is_ascii_digit() {
                any = false;
                break;
            }
            idx = idx * 10 + (d - b'0') as usize;
            any = true;
        }
        if any {
            let cap = crate::start_cap(idx);
            return (cap != crate::NO_CAP).then_some(cap);
        }
    }
    None
}

/// Окно клиента: право на композитор плюс выданный им номер.
pub struct Window {
    ep: usize,
    id: u32,
    pub width: u16,
    pub height: u16,
}

impl Window {
    /// Попросить окно. `None` — композитора нет или он отказал.
    pub fn create(width: u16, height: u16, title: &str) -> Option<Window> {
        let ep = endpoint()?;
        let mut req = [0u8; 4 + TITLE_MAX];
        req[0..2].copy_from_slice(&width.to_le_bytes());
        req[2..4].copy_from_slice(&height.to_le_bytes());
        let t = title.as_bytes();
        let n = t.len().min(TITLE_MAX);
        req[4..4 + n].copy_from_slice(&t[..n]);
        let mut rep = [0u8; 4];
        if crate::call(ep, OP_CREATE, &req[..4 + n], &mut rep) != 4 {
            return None;
        }
        Some(Window { ep, id: u32::from_le_bytes(rep), width, height })
    }

    /// Отдать композитору новый кадр: положить пиксели объектом и назвать его.
    ///
    /// `pixels` — RGBA8888 по строкам, ровно `width * height * 4` байта. Кладём через store, а
    /// не шлём по IPC, ровно по доводу из шапки: содержимое адресуется хэшем, а не местом.
    pub fn present(&self, store: usize, pixels: &[u8]) -> bool {
        self.present_rect(store, pixels, 0, 0, self.width, self.height)
    }

    /// Отдать ЧАСТЬ кадра: `pixels` — ровно `w*h*4` байта прямоугольника `(x, y, w, h)`.
    ///
    /// Это основной путь для всего, что перерисовывается часто: платит только за изменившееся.
    pub fn present_rect(
        &self, store: usize, pixels: &[u8], x: u16, y: u16, w: u16, h: u16,
    ) -> bool {
        let mut id = [0u8; 32];
        if crate::obj_put(store, pixels, &mut id) != 0 {
            return false;
        }
        let mut req = [0u8; 44];
        req[0..4].copy_from_slice(&self.id.to_le_bytes());
        req[4..6].copy_from_slice(&x.to_le_bytes());
        req[6..8].copy_from_slice(&y.to_le_bytes());
        req[8..10].copy_from_slice(&w.to_le_bytes());
        req[10..12].copy_from_slice(&h.to_le_bytes());
        req[12..44].copy_from_slice(&id);
        if crate::call(self.ep, OP_ATTACH, &req, &mut []) == crate::NO_CAP {
            return false;
        }
        self.damage(x, y, w, h)
    }

    /// Сказать «кадр готов» и объявить изменившийся прямоугольник.
    pub fn damage(&self, x: u16, y: u16, w: u16, h: u16) -> bool {
        let mut req = [0u8; 12];
        req[0..4].copy_from_slice(&self.id.to_le_bytes());
        req[4..6].copy_from_slice(&x.to_le_bytes());
        req[6..8].copy_from_slice(&y.to_le_bytes());
        req[8..10].copy_from_slice(&w.to_le_bytes());
        req[10..12].copy_from_slice(&h.to_le_bytes());
        crate::call(self.ep, OP_COMMIT, &req, &mut []) != crate::NO_CAP
    }

    /// Ждать событие. Ответ отложенный: пока событий нет, клиент спит в `SYS_CALL` — ровно так
    /// же, как программа спит в чтении stdin у терминала.
    pub fn next_event(&self) -> Option<Event> {
        self.event(OP_EVENT)
    }

    /// Забрать событие, если оно уже есть. `None` — событий нет ПРЯМО СЕЙЧАС.
    pub fn poll_event(&self) -> Option<Event> {
        self.event(OP_POLL)
    }

    /// Сказать композитору, что окна больше не будет.
    pub fn destroy(&self) {
        crate::call(self.ep, OP_DESTROY, &self.id.to_le_bytes(), &mut []);
    }

    fn event(&self, op: usize) -> Option<Event> {
        let mut rep = [0u8; 16];
        let n = crate::call(self.ep, op, &self.id.to_le_bytes(), &mut rep);
        if n == 0 || n == crate::NO_CAP {
            return None;
        }
        Event::parse(&rep[..n])
    }
}
