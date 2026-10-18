//! Цикл программы: нарисовать кадр — дождаться события — повторить.
//!
//! Событий ровно четыре, и три из них не клавиши. Так вышло не от бедности: сигналов в VOID нет
//! и не будет ([[process-stdio]]), поэтому «окно стало другим» и «тебя прервали» приезжают
//! ВИДОМ ОТВЕТА на чтение ввода — и программе приходится их различать. Здесь они названы, чтобы
//! их нельзя было молча проглотить.

use void_user as sys;
use void_user::stdio;

use super::key::{Key, Keys};
use super::screen::Screen;

/// Что случилось.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Key(Key),
    /// Размер окна сменился — экран уже перемерен, кадр надо нарисовать заново.
    Resized,
    /// Прервали (Ctrl+C). Решает программа: редактор спросит про несохранённое, счётчик бросит
    /// счёт. Выход по умолчанию — дело зовущего, а не наше.
    Interrupted,
    /// Ввода больше не будет: хост ушёл, труба закрылась.
    Eof,
}

/// Полноэкранная программа: экран и разбор ввода в одном.
pub struct App {
    screen: Screen,
    keys: Keys,
    buf: [u8; 64],
    /// Клавиши, собранные из последнего чтения и ещё не отданные.
    ready: [Option<Key>; 64],
    n: usize,
    at: usize,
}

impl App {
    /// Войти в полноэкранный режим.
    pub fn open() -> App {
        App {
            screen: Screen::open(),
            keys: Keys::default(),
            buf: [0; 64],
            ready: [None; 64],
            n: 0,
            at: 0,
        }
    }

    pub fn screen(&mut self) -> &mut Screen {
        &mut self.screen
    }

    /// Съехать с экрана. Зовут перед `sys::exit` — иначе человек останется в чужом кадре.
    pub fn close(&mut self) {
        self.screen.close();
    }

    /// Дождаться следующего события.
    ///
    /// Чтение БЛОКИРУЮЩЕЕ, и это главное свойство цикла: программа, которая опрашивает ввод,
    /// не даёт системе спать, а за сон в VOID платят временем батареи ([[cpu-budget]]). Пока
    /// человек не нажал клавишу и окно не изменилось, процесс не исполняется вовсе.
    pub fn next(&mut self) -> Event {
        loop {
            // Сначала раздаём то, что уже разобрано: одно чтение приносит несколько клавиш
            // (вставка из буфера, быстрый набор, автоповтор).
            while self.at < self.n {
                let k = self.ready[self.at].take();
                self.at += 1;
                if let Some(k) = k {
                    return Event::Key(k);
                }
            }
            self.n = 0;
            self.at = 0;
            let got = match stdio::read_ex(&mut self.buf) {
                stdio::In::Bytes(k) => k,
                stdio::In::Eof => return Event::Eof,
                stdio::In::Interrupted => return Event::Interrupted,
                stdio::In::Resized => {
                    // Перемеряем ЗДЕСЬ, а не в программе: забыть это — значит рисовать в старых
                    // границах, и выглядит такое как поломка вывода, а не как забытый вызов.
                    self.screen.measure();
                    return Event::Resized;
                }
                // Хоста stdio нет (спасательный шелл) — читаем консолью ядра.
                stdio::In::NoHost => sys::read_stdin(&mut self.buf),
            };
            if got == 0 {
                return Event::Eof;
            }
            for &b in &self.buf[..got.min(self.buf.len())] {
                if let Some(k) = self.keys.feed(b) {
                    if self.n < self.ready.len() {
                        self.ready[self.n] = Some(k);
                        self.n += 1;
                    }
                }
            }
        }
    }
}
