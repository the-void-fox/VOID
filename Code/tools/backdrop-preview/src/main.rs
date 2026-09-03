//! Хостовый предпросмотр стола: `backdrop-preview [файл.png [ширина высота]]`.
//!
//! Рисует ТЕМ ЖЕ `programs/user/src/backdrop.rs`, что едет в систему, — файл включён по пути,
//! копии нет. Смысл остался прежним (Веха 141: правку видно за секунду, а не за загрузку QEMU),
//! но предмет сменился: до Вехи 158 здесь смотрели геометрию ЗНАКА VOID во весь экран, теперь
//! знака на столе нет, и смотреть остаётся ровность виньетки и рассеивания на больших экранах —
//! на 4K полосы от округления заметны, а на 1280×800 нет.
//!
//! Иконки предпросматривает не этот инструмент, а `svg2vg --preview`: они рисуются `void-vec`.
//!
//! PNG пишется несжатым (deflate «stored»): кодировщик общий с `svg2vg` — `tools/png-write.rs`,
//! включён по пути, копии нет.
extern crate alloc;

#[path = "../../../programs/user/src/backdrop.rs"]
mod backdrop;

#[path = "../../png-write.rs"]
mod png;

use png::write_png;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "preview.png".into());
    let w: u32 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1280);
    let h: u32 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(800);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let t = std::time::Instant::now();
    backdrop::wallpaper(&mut px, w, h, &backdrop::Palette::VOID);
    eprintln!("{w}×{h} за {:?}", t.elapsed());
    write_png(&out, &px, w, h);
}
