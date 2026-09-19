//! Протокол звукового сервера и клиентская обёртка над ним (Веха 202.2).
//!
//! Одно место, где записан формат сообщений к `hda`: **и сервер, и клиенты берут константы
//! отсюда**. Тот же порядок, что у [`crate::net_cli`], и по той же причине — номера операций,
//! разъехавшиеся по трём файлам, расходятся молча.
//!
//! Право играть звук — это **cap на эндпоинт `hda`** (`endpoint:hda` в конфиге системы). Своего
//! отдельного права у «звука» нет: у кого есть канал к серверу, тот может играть. В отличие от
//! экрана и выключения питания, канал НЕ помечен «не наследуется» — звук получает каждое окно,
//! и это осознанно: играть умеет любая программа в любой системе, а прятать за правом то, что
//! можно сделать тремя строками в своём процессе, значит не защитить, а усложнить.
//!
//! Формат ответа единый: **`[status(1) | …]`**, где `0` — принято, `1` — отказ (негодный
//! запрос), `2` — звука в системе нет (контроллер не поднялся), `3` — поток занят другим.

use crate as sys;

/// Сыграть тон: `[частота(u32) | миллисекунды(u32)]`.
///
/// Тон, а не файл, потому что это и есть системный звук: короткий сигнал, который нужен
/// уведомлению, ошибке ввода и концу долгой работы. Ему не нужен ни файл, ни разбор формата,
/// ни память под звуковую дорожку — только два числа.
pub const OP_BEEP: usize = 1;

/// Тишина немедленно: оборвать то, что играет.
pub const OP_HUSH: usize = 2;

/// Открыть поток: `[частота(u32) | имя…]` + право на кольцо (общая память). Кольцо заполняет
/// клиент, читает сервер; синхронизация — через [`OP_ADVANCE`].
///
/// Почему кольцо в общей памяти, а не данные в сообщениях: секунда звука это 192 килобайта, а
/// сообщение у нас — сотни байт. Гнать дорожку сообщениями значило бы делать по вызову ядра на
/// каждые три миллисекунды звука.
///
/// Веха 204 — ИМЯ хвостом сообщения: то, под которым голос виден в миксере. Имя называет САМ
/// клиент, и это осознанно: спрашивать его у ядра значило бы дать звуковому серверу право
/// обзора процессов ради подписи в меню. Соврать им можно ровно настолько, насколько можно
/// соврать именем своей программы, — а подделать его нельзя, не подменив саму программу.
pub const OP_OPEN: usize = 3;

/// «Я дописал до этого места»: `[позиция записи(u32)]` → `[статус | позиция чтения(u32)]`.
///
/// Обе позиции — счётчики в байтах от начала потока, не индексы в кольце: так не нужно
/// различать «кольцо пусто» и «кольцо полно», а переполнение счётчика наступает через сутки
/// непрерывного звука и обрабатывается вычитанием по модулю.
pub const OP_ADVANCE: usize = 4;

/// Закончить: доиграть то, что уже в кольце, и отпустить общую память.
pub const OP_CLOSE: usize = 5;

/// Веха 204 — СОСТОЯНИЕ МИКСЕРА: `[]` → мастер, имя выхода и все играющие голоса.
///
/// Спрашивает его панель, чтобы нарисовать меню звука. Ответ разбирает [`parse_state`].
pub const OP_STATE: usize = 6;

/// Веха 204 — поставить громкость: `[id(u16) | громкость(u8)]`. `id == 0` — МАСТЕР (общая),
/// иначе номер голоса из [`State`].
///
/// Громкость в процентах, а не в децибелах: в меню у неё ползунок и подпись «50 %», и переводить
/// одно в другое дважды (здесь и там) значит однажды перевести по-разному.
pub const OP_VOLUME: usize = 7;

/// Веха 204 — СПИСОК ВЫХОДОВ кодека: `[]` → сколько их, какой выбран и как называются.
pub const OP_OUTPUTS: usize = 8;

/// Веха 204 — ВЫБРАТЬ ВЫХОД: `[номер(u8)]` из списка [`OP_OUTPUTS`].
pub const OP_PICK: usize = 9;

/// Размер кольца: половина секунды при 48 кГц / 16 бит / 2 канала. Больше не нужно (задержка
/// между «передумал» и тишиной), меньше — заставляет клиента просыпаться чаще, чем стоит звук.
pub const RING_BYTES: usize = 192 * 1024;

pub const ST_OK: u8 = 0;
pub const ST_BAD: u8 = 1;
pub const ST_NO_SOUND: u8 = 2;
/// Свободных голосов у миксера не осталось (см. [`MAX_VOICES`]). До Вехи 204 это значило
/// «поток уже отдан другому»: микшера не было вовсе, и второй звук просто некуда было деть.
pub const ST_BUSY: u8 = 3;

/// Сколько голосов сервер смешивает одновременно.
///
/// Восемь — это не про мощность процессора (сложить восемь чисел на отсчёт дешевле, чем их
/// прочитать), а про место: у каждого голоса своё кольцо в общей памяти по 192 КиБ, и каждое
/// отображается в адресное пространство сервера.
pub const MAX_VOICES: usize = 8;

/// Предел имени голоса в байтах. Обрезается по границе символа — кириллица в UTF-8 двухбайтная,
/// и обрубок посреди буквы нарисовался бы ромбиком.
///
/// Тридцать два, а не двадцать четыре: на двадцати четырёх не помещалось даже имя выхода
/// («линейный выход» — это 27 байт), и меню показывало «линейный вых».
pub const NAME_MAX: usize = 32;

/// Длина ответа на [`OP_STATE`]: заголовок и все голоса подряд, всё фиксированного размера.
pub const STATE_BYTES: usize = 3 + (1 + NAME_MAX) + MAX_VOICES * (4 + NAME_MAX);

/// Один голос миксера — то, что видно в меню строкой с ползунком.
#[derive(Clone, Copy)]
pub struct Voice {
    /// Номер голоса у сервера. Держится живым, пока голос играет; исчез из списка — голос умер.
    pub id: u16,
    /// Громкость в процентах.
    pub vol: u8,
    pub name: [u8; NAME_MAX],
    pub name_len: u8,
}

impl Voice {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
}

/// Состояние миксера целиком: что отдаёт [`OP_STATE`].
#[derive(Clone, Copy)]
pub struct State {
    /// Общая громкость в процентах.
    pub master: u8,
    /// Играет ли сейчас хоть что-то (для значка в панели).
    pub playing: bool,
    /// Имя выхода — то, во что сервер ведёт звук: «Динамики», «Наушники».
    pub out: [u8; NAME_MAX],
    pub out_len: u8,
    pub count: usize,
    pub voices: [Voice; MAX_VOICES],
}

impl State {
    pub fn out(&self) -> &str {
        core::str::from_utf8(&self.out[..self.out_len as usize]).unwrap_or("?")
    }
    pub fn voices(&self) -> &[Voice] {
        &self.voices[..self.count]
    }
}

/// Разобрать ответ [`OP_STATE`]. `None` — сервер ответил не тем или не ответил вовсе.
///
/// Разбор живёт ЗДЕСЬ, рядом со сборкой на стороне сервера: формат с переменным числом записей
/// читается и пишется в двух местах, и разъехаться им можно только молча.
pub fn parse_state(b: &[u8]) -> Option<State> {
    if b.len() < 4 + NAME_MAX || b[0] != ST_OK {
        return None;
    }
    let mut st = State {
        master: b[1],
        playing: b[2] & 1 != 0,
        out: [0; NAME_MAX],
        out_len: b[3].min(NAME_MAX as u8),
        count: 0,
        voices: [Voice { id: 0, vol: 0, name: [0; NAME_MAX], name_len: 0 }; MAX_VOICES],
    };
    st.out.copy_from_slice(&b[4..4 + NAME_MAX]);
    let mut at = 4 + NAME_MAX;
    while at + 4 + NAME_MAX <= b.len() && st.count < MAX_VOICES {
        let id = u16::from_le_bytes([b[at], b[at + 1]]);
        if id == 0 {
            break; // нулевой номер — конец списка: голоса нумеруются с единицы
        }
        let v = &mut st.voices[st.count];
        v.id = id;
        v.vol = b[at + 2];
        v.name_len = b[at + 3].min(NAME_MAX as u8);
        v.name.copy_from_slice(&b[at + 4..at + 4 + NAME_MAX]);
        st.count += 1;
        at += 4 + NAME_MAX;
    }
    Some(st)
}

/// Предел длительности одного сигнала. Системный звук длиннее секунды — это уже не сигнал, а
/// помеха; а ещё это защита от опечатки в аргументе, которая иначе загудела бы на полчаса.
pub const MAX_MS: u32 = 2000;

/// Сыграть тон. Возвращает статус сервера; ждать конца звука не нужно — сервер отвечает сразу,
/// как принял, и играет сам.
pub fn beep(ep: usize, hz: u32, ms: u32) -> u8 {
    let mut body = [0u8; 8];
    body[..4].copy_from_slice(&hz.to_le_bytes());
    body[4..].copy_from_slice(&ms.to_le_bytes());
    let mut rsp = [0u8; 4];
    let n = sys::call(ep, OP_BEEP, &body, &mut rsp);
    // Вызов не состоялся вовсе — сервера за каналом нет (машина без звуковой карты: сервис
    // поднялся, не получил окна регистров и вышел). Это НЕ отказ по существу просьбы, и путать
    // их нельзя: человек иначе читает «частота вне диапазона» там, где звука нет в принципе.
    if n == usize::MAX || n == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Оборвать звук.
pub fn hush(ep: usize) -> u8 {
    let mut rsp = [0u8; 4];
    let n = sys::call(ep, OP_HUSH, &[], &mut rsp);
    if n == usize::MAX || n == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Открыть поток на своё кольцо. `cap` — право на общую память (обычно урезанное до чтения),
/// `name` — под каким именем голос будет виден в миксере (Веха 204).
pub fn open(ep: usize, rate: u32, cap: usize, name: &str) -> u8 {
    let mut body = [0u8; 4 + NAME_MAX];
    body[..4].copy_from_slice(&rate.to_le_bytes());
    let n = fit(name);
    body[4..4 + n.len()].copy_from_slice(n.as_bytes());
    let mut rsp = [0u8; 4];
    let (r, _) = sys::call_full(ep, OP_OPEN, &body[..4 + n.len()], &mut rsp, cap);
    if r == usize::MAX || r == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Обрезать имя до [`NAME_MAX`] байт ПО ГРАНИЦЕ СИМВОЛА: в UTF-8 кириллица двухбайтная, и
/// обрубок посреди буквы — это ромбик в меню, а не сокращение.
fn fit(name: &str) -> &str {
    if name.len() <= NAME_MAX {
        return name;
    }
    let mut end = NAME_MAX;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Выходы звуковой карты: куда она умеет играть и куда играет сейчас.
#[derive(Clone, Copy)]
pub struct Outputs {
    pub count: usize,
    /// Номер выбранного — индекс в [`Outputs::names`].
    pub cur: usize,
    pub names: [[u8; NAME_MAX]; MAX_VOICES],
    pub lens: [u8; MAX_VOICES],
}

impl Outputs {
    pub fn name(&self, i: usize) -> &str {
        core::str::from_utf8(&self.names[i][..self.lens[i] as usize]).unwrap_or("?")
    }
}

/// Веха 204 — какие выходы есть у карты и какой выбран.
pub fn outputs(ep: usize) -> Option<Outputs> {
    let mut rsp = [0u8; 2 + MAX_VOICES * (1 + NAME_MAX) + 1];
    let n = sys::call(ep, OP_OUTPUTS, &[], &mut rsp);
    if n == usize::MAX || n < 3 || rsp[0] != ST_OK {
        return None;
    }
    let mut out = Outputs {
        count: (rsp[1] as usize).min(MAX_VOICES),
        cur: rsp[2] as usize,
        names: [[0; NAME_MAX]; MAX_VOICES],
        lens: [0; MAX_VOICES],
    };
    let mut at = 3;
    for i in 0..out.count {
        if at + 1 + NAME_MAX > n {
            out.count = i;
            break;
        }
        out.lens[i] = rsp[at].min(NAME_MAX as u8);
        out.names[i].copy_from_slice(&rsp[at + 1..at + 1 + NAME_MAX]);
        at += 1 + NAME_MAX;
    }
    Some(out)
}

/// Веха 204 — играть в выход с этим номером.
pub fn pick(ep: usize, i: usize) -> u8 {
    let mut rsp = [0u8; 4];
    let n = sys::call(ep, OP_PICK, &[i as u8], &mut rsp);
    if n == usize::MAX || n == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Веха 204 — спросить состояние миксера: общая громкость, выход и голоса.
pub fn state(ep: usize) -> Option<State> {
    let mut rsp = [0u8; STATE_BYTES];
    let n = sys::call(ep, OP_STATE, &[], &mut rsp);
    if n == usize::MAX || n == 0 {
        return None;
    }
    parse_state(&rsp[..n.min(STATE_BYTES)])
}

/// Веха 204 — поставить громкость голосу (`id`) или общую (`id == 0`).
pub fn volume(ep: usize, id: u16, vol: u8) -> u8 {
    let mut body = [0u8; 3];
    body[..2].copy_from_slice(&id.to_le_bytes());
    body[2] = vol.min(100);
    let mut rsp = [0u8; 4];
    let n = sys::call(ep, OP_VOLUME, &body, &mut rsp);
    if n == usize::MAX || n == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Сказать, докуда дописали, и узнать, докуда сервер дочитал. `None` — сервер не ответил.
pub fn advance(ep: usize, write: u32) -> Option<(u8, u32)> {
    let mut rsp = [0u8; 8];
    let n = sys::call(ep, OP_ADVANCE, &write.to_le_bytes(), &mut rsp);
    if n == usize::MAX || n < 5 {
        return None;
    }
    Some((rsp[0], u32::from_le_bytes([rsp[1], rsp[2], rsp[3], rsp[4]])))
}

/// Закончить поток.
pub fn close(ep: usize) -> u8 {
    let mut rsp = [0u8; 4];
    let n = sys::call(ep, OP_CLOSE, &[], &mut rsp);
    if n == usize::MAX || n == 0 {
        ST_NO_SOUND
    } else {
        rsp[0]
    }
}

/// Найти канал к звуку среди своих прав. `None` — звука у нас нет, и это нормальный случай:
/// система без звуковой карты, поколение без строки `service hda`, окно без унаследованного
/// права. Спрашивать надо ПО ИМЕНИ: позиция прав меняется от поколения к поколению, и звук,
/// взятый по номеру, однажды окажется чужим эндпоинтом (так мы уже теряли сеть, Веха 199.14).
pub fn find_cap() -> Option<usize> {
    sys::cap_named("HDA")
}
