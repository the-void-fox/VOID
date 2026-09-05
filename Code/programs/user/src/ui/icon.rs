//! Иконки системы: векторные фигуры `.vg`, вшитые в бинарь (Веха 158).
//!
//! ## Почему в бинаре, а не в store
//!
//! По той же причине, по которой стол рисуется формулами: панель обязана подняться на машине,
//! где в store ещё нет ничего. Иконка из store зависела бы от того, что кто-то её туда положил,
//! от прав на объект и от того, что объект не побился, — три способа остаться без интерфейса
//! ради экономии килобайта. Все иконки вместе весят меньше двух килобайт.
//!
//! ## Откуда они берутся
//!
//! `Code/assets/icons/<имя>.svg` — выгрузка кадра из макета; рядом `<имя>.vg`, который делает
//! `Code/tools/svg2vg`. Правится ВСЕГДА `.svg`, `.vg` пересобирается:
//!
//! ```text
//! svg2vg --out Code/assets/icons --preview /tmp/icons Code/assets/icons/*.svg
//! ```
//!
//! Оба файла лежат в репозитории намеренно. Гнать конвертер из сборки системы значило бы тянуть
//! usvg в цепочку, которая обязана собираться оффлайн; а держать только `.vg` — потерять
//! источник, из которого его можно пересобрать.

/// Знак системы: перечёркнутый глаз. Меню оболочки, приветственное окно.
pub const LOGO: &[u8] = include_bytes!("../../../../assets/icons/logo.vg");
/// Выключение.
pub const POWER: &[u8] = include_bytes!("../../../../assets/icons/power.vg");
/// Загрузка процессора.
pub const CPU: &[u8] = include_bytes!("../../../../assets/icons/cpu.vg");
/// Занятая память.
pub const RAM: &[u8] = include_bytes!("../../../../assets/icons/ram.vg");
/// Веха 166 — знаки ФАЙЛОВОГО МЕНЕДЖЕРА, из пакета владельца (Material Symbols Rounded 24,
/// `Reference/Design/Assets`, вес 400 — он один и подходит к нашему дизайну). Взяты как есть,
/// без правок: пакет отобран владельцем именно под эту оболочку.
pub const FOLDER: &[u8] = include_bytes!("../../../../assets/icons/folder.vg");
pub const FILE: &[u8] = include_bytes!("../../../../assets/icons/file.vg");
pub const BACK: &[u8] = include_bytes!("../../../../assets/icons/back.vg");
pub const FORWARD: &[u8] = include_bytes!("../../../../assets/icons/forward.vg");
pub const UP: &[u8] = include_bytes!("../../../../assets/icons/up.vg");
pub const STAR: &[u8] = include_bytes!("../../../../assets/icons/star.vg");
pub const SEARCH: &[u8] = include_bytes!("../../../../assets/icons/search.vg");
pub const MENU: &[u8] = include_bytes!("../../../../assets/icons/menu.vg");

/// Веха 168 — уведомления: колокольчик в панели и крестик «убрать».
pub const BELL: &[u8] = include_bytes!("../../../../assets/icons/bell.vg");
pub const CLOSE: &[u8] = include_bytes!("../../../../assets/icons/close.vg");

/// Веха 165 — ОТНЯТЬ ПРАВО: перечёркнутая рамка. В макете диспетчера на месте кнопки «отнять»
/// стоит знак — строка права коротка, и слово в ней занимает больше места, чем сама строка.
///
/// Нарисован здесь, а не выгружен из Figma: доступ к макету упёрся в потолок обращений, а знак
/// в макете размером 11 точек и состоит из рамки и креста. Когда потолок отпустит, его надо
/// СВЕРИТЬ с оригиналом — это единственная иконка системы, у которой источник не выгрузка.
pub const REVOKE: &[u8] = include_bytes!("../../../../assets/icons/revoke.vg");
