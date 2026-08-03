//! # ereb-mux
//!
//! Мультиплексор: панели (тайлинг), вкладки и (позже) сессии — слой, который
//! делает из ereb «Zellij», а не просто терминал.
//!
//! Этот этап (v0.3/Этап 12) начинается с фундамента — чистой геометрии тайлинга
//! ([`SplitTree`] → [`PaneRect`]). Панели (PTY + терминал-ядро), рендер
//! нескольких гридов, фокус/навигация и поллинг нескольких PTY навешиваются
//! поверх. См. `obsidian/03-subsystems/multiplexer.md`.

// Раскладка панелей — чистая геометрия, ОС не нужна: `no_std`, чтобы крейт ехал на VOID
// ([ADR 0005]).
//
// [ADR 0005]: ../../../obsidian/02-architecture/adr/0005-void-target.md
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod layout;
mod nav;

pub use layout::{Area, PaneId, PaneRect, SplitDirection, SplitTree};
pub use nav::{Direction, neighbor};
