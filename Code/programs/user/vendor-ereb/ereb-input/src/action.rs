//! [`Action`] — результат интерпретации клавиши, и [`Mode`] биндингов.

/// Режим ввода (для префиксных биндингов в духе Zellij/tmux).
///
/// Переходы между режимами (state machine) — задача `ereb-mux` (Этап 13);
/// здесь только перечисление и хранение в таблице биндингов.
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Mode {
    #[default]
    Normal,
    Pane,
    Tab,
    Scroll,
}

/// Что делать в ответ на клавишу.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Отправить байты в PTY (дефолт для всего, что не перехвачено биндингом).
    SendToPty(Vec<u8>),
    /// Скопировать выделение в буфер обмена.
    Copy,
    /// Вставить из буфера обмена.
    Paste,
    /// Новая вкладка.
    NewTab,
    /// Новая панель.
    NewPane,
    /// Следующая панель.
    NextPane,
    /// Переключиться в режим биндингов.
    EnterMode(Mode),
    /// Действие, определяемое плагином.
    Custom(String),
    /// Ничего не делать (клавиша поглощена, но без эффекта).
    Ignore,
}
