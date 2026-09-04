//! SVG → `.vg`.
//!
//! ```text
//! svg2vg --out <каталог> <файл.svg> [ещё.svg ...]
//! svg2vg --out <каталог> --box 21x21 icons/*.svg     # свой вьюбокс вместо размера из SVG
//! svg2vg --out <каталог> --preview <каталог> ...     # ещё и PNG для глаз
//! ```
//!
//! `--preview` рисует получившийся `.vg` НАШИМ ЖЕ растеризатором (`void-vec`) и кладёт PNG.
//! Это не украшение: так видно результат до сборки образа и загрузки QEMU, и проверяется вся
//! цепочка целиком — запись, чтение и растеризация, а не только конвертер.
//!
//! ## Что теряется — и почему об этом кричим
//!
//! Формат `.vg` знает только сплошную заливку. Всё остальное — градиенты, узоры, картинки,
//! текст, обтравку, маски, фильтры — конвертер пропускает и ПИШЕТ ОБ ЭТОМ в stderr, а в конце
//! возвращает ненулевой код. Молча выкидывать нельзя: иконка с градиентом превратилась бы в
//! иконку без градиента, и заметили бы это в лучшем случае на экране, в худшем — на видео.
//!
//! ## Порядок рисования
//!
//! Фигуры выкладываются ровно в том порядке, в каком usvg обходит дерево, — это и есть порядок
//! SVG. У пути с заливкой И обводкой порядок берётся из `paint-order`, а не «сначала заливка»:
//! иначе обводка, которая по замыслу под заливкой, легла бы поверх.
//!
//! ## Прозрачность
//!
//! Непрозрачность группы в формате хранить негде, поэтому она вмножается в альфу каждой фигуры
//! внутри. Это НЕ то же самое, что настоящая групповая прозрачность (там слой сначала рисуется
//! целиком, а потом гасится, и перекрытия внутри группы не просвечивают друг через друга), —
//! но для иконок, где фигуры не наезжают, разницы нет. Если наедут, будет видно, и это честнее
//! молчаливого «поддерживаем».

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tiny_skia_path::PathSegment;
use usvg::{Node, Paint, PaintOrder};

#[path = "../../png-write.rs"]
mod png;

/// Фон предпросмотра — цвет панели из дизайна: иконки в системе лежат на нём, и на белом
/// или на прозрачном их светлота обманывала бы.
const PREVIEW_BG: [u8; 4] = [0x3f, 0x3f, 0x3f, 0xff];

fn main() -> ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut preview: Option<PathBuf> = None;
    let mut scale: u32 = 8;
    let mut vbox: Option<(u16, u16)> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" | "-o" => match args.next() {
                Some(v) => out = Some(PathBuf::from(v)),
                None => return usage("после --out нужен каталог"),
            },
            "--preview" => match args.next() {
                Some(v) => preview = Some(PathBuf::from(v)),
                None => return usage("после --preview нужен каталог"),
            },
            "--preview-scale" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v @ 1..=64) => scale = v,
                _ => return usage("после --preview-scale нужно число 1..64"),
            },
            "--box" => match args.next().as_deref().and_then(parse_box) {
                Some(v) => vbox = Some(v),
                None => return usage("после --box нужен размер вида 21x21"),
            },
            "--help" | "-h" => return usage(""),
            s if s.starts_with('-') => return usage(&format!("неизвестный ключ {s}")),
            s => inputs.push(PathBuf::from(s)),
        }
    }

    let Some(out) = out else { return usage("не задан --out") };
    if inputs.is_empty() {
        return usage("не задано ни одного SVG");
    }
    for d in [Some(&out), preview.as_ref()].into_iter().flatten() {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("не создать {}: {e}", d.display());
            return ExitCode::FAILURE;
        }
    }

    let mut lost = false;
    for src in &inputs {
        match convert(src, vbox) {
            Ok((bytes, shapes, skipped)) => {
                let name = src.file_stem().unwrap_or_default();
                let dst = out.join(name).with_extension("vg");
                if let Err(e) = std::fs::write(&dst, &bytes) {
                    eprintln!("не записать {}: {e}", dst.display());
                    return ExitCode::FAILURE;
                }
                println!("{} → {}  ({shapes} фигур, {} Б)", src.display(), dst.display(), bytes.len());
                if let Some(dir) = &preview {
                    let p = dir.join(name).with_extension("png");
                    render_preview(&bytes, &p, scale);
                }
                lost |= skipped;
            }
            Err(e) => {
                eprintln!("{}: {e}", src.display());
                return ExitCode::FAILURE;
            }
        }
    }
    // Ненулевой код при потерях — чтобы сборка иконок падала, а не проходила с предупреждением
    // в середине простыни вывода.
    if lost {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn usage(err: &str) -> ExitCode {
    if !err.is_empty() {
        eprintln!("svg2vg: {err}");
    }
    eprintln!("использование: svg2vg --out <каталог> [--box ШxВ] <файл.svg> ...");
    if err.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse_box(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once(['x', 'X', '×'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Предпросмотр: читаем ТОЛЬКО ЧТО записанные байты обратно и рисуем растеризатором системы.
/// Именно обратно, а не «из того, что было в памяти», — иначе ошибка в записи или в чтении
/// формата осталась бы незамеченной до первой загрузки.
fn render_preview(bytes: &[u8], dst: &Path, scale: u32) {
    let Some(vg) = void_vec::Vg::parse(bytes) else {
        eprintln!("  предпросмотр: свои же байты не читаются — это ошибка формата");
        return;
    };
    let (vw, vh) = vg.size();
    let (w, h) = (vw as u32 * scale, vh as u32 * scale);
    let mut px: Vec<u8> = PREVIEW_BG.iter().copied().cycle().take((w * h * 4) as usize).collect();
    if let Some(mut c) = void_vec::Canvas::new(&mut px, w as usize, h as usize) {
        c.draw(&vg, void_vec::Fit::new(0, 0, w, h), None);
    }
    png::write_png(&dst.to_string_lossy(), &px, w, h);
}

/// Возвращает (байты, число фигур, были ли потери).
fn convert(src: &Path, vbox: Option<(u16, u16)>) -> Result<(Vec<u8>, u16, bool), String> {
    let data = std::fs::read(src).map_err(|e| format!("не прочитать: {e}"))?;
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(&data, &opt).map_err(|e| format!("не разобрать SVG: {e}"))?;

    let size = tree.size();
    let (vw, vh) = match vbox {
        Some(v) => v,
        None => {
            let (w, h) = (size.width(), size.height());
            if (w - w.round()).abs() > 0.01 || (h - h.round()).abs() > 0.01 {
                eprintln!(
                    "  {}: размер {w}×{h} дробный — вьюбокс округляю вверх, вписывание сместится \
                     на доли пикселя (задай --box, если это важно)",
                    src.display()
                );
            }
            (w.ceil().max(1.0) as u16, h.ceil().max(1.0) as u16)
        }
    };

    let mut w = void_vec::Writer::new();
    let mut st = Walk { w: &mut w, n: 0, lost: false, src, first_box: None };
    st.group(tree.root(), 1.0);
    let (n, lost) = (st.n, st.lost);

    // Подсказка про ПОДЛОЖКУ КАДРА. Выгрузка кадра из редактора тащит с собой его заливку
    // отдельной фигурой во весь вьюбокс, а иконку система красит ЦЕЛИКОМ в цвет темы — и такая
    // подложка закрашивает собой глиф. Ловится это не на глаз, а на экране, поэтому говорим тут.
    //
    // Только предупреждение: выкидывать самим нельзя — фигура во весь вьюбокс бывает и настоящей
    // (залитый круг с вырезом, плашка). Решает человек, а дело инструмента — не дать пропустить.
    //
    // Условие включает `n > 1`: у иконки из ОДНОЙ фигуры перекрывать нечего, а габарит
    // единственного глифа сплошь и рядом совпадает с вьюбоксом — выгрузка обрезана по нему.
    if let Some(b) = st.first_box.filter(|_| n > 1) {
        if b.left() <= 0.5
            && b.top() <= 0.5
            && b.right() >= size.width() - 0.5
            && b.bottom() >= size.height() - 0.5
        {
            eprintln!(
                "  {}: первая фигура закрывает весь вьюбокс — похоже на подложку кадра. \
                 Иконка красится в один цвет темы, и подложка закроет собой рисунок: \
                 если это она, удали её <rect> из SVG",
                src.display()
            );
        }
    }

    let bytes = w
        .finish(vw, vh)
        .ok_or_else(|| String::from("в файле не осталось ни одной заливки"))?;
    Ok((bytes, n, lost))
}

struct Walk<'a, 'b> {
    w: &'a mut void_vec::Writer,
    n: u16,
    lost: bool,
    src: &'b Path,
    /// Габарит ПЕРВОЙ выложенной фигуры — по нему узнаётся подложка кадра (см. `convert`).
    first_box: Option<tiny_skia_path::Rect>,
}

impl Walk<'_, '_> {
    fn skip(&mut self, what: &str) {
        self.lost = true;
        eprintln!("  {}: ПРОПУЩЕНО — {what}", self.src.display());
    }

    fn group(&mut self, g: &usvg::Group, alpha: f32) {
        // Обтравка, маска и фильтры меняют картинку так, что «почти получилось» не бывает:
        // либо мы их считаем, либо результат другой. Считать в системе нечем — значит говорим.
        if g.clip_path().is_some() {
            self.skip("обтравка (clip-path) — фигуры внутри выйдут за свои границы");
        }
        if g.mask().is_some() {
            self.skip("маска (mask) — фигуры внутри нарисуются целиком");
        }
        if !g.filters().is_empty() {
            self.skip("фильтр (filter) — размытия и тени формат не хранит");
        }
        let alpha = alpha * g.opacity().get();
        for node in g.children() {
            match node {
                Node::Group(inner) => self.group(inner, alpha),
                Node::Path(p) => self.path(p, alpha),
                Node::Image(_) => self.skip("растровая картинка внутри SVG"),
                Node::Text(_) => self.skip("текст (переведи его в контуры в редакторе)"),
            }
        }
    }

    fn path(&mut self, p: &usvg::Path, alpha: f32) {
        if !p.is_visible() {
            return;
        }
        // paint-order решает, что ложится сверху. Значение по умолчанию — заливка, потом обводка.
        let fill_first = !matches!(p.paint_order(), PaintOrder::StrokeAndFill);
        if fill_first {
            self.fill(p, alpha);
            self.stroke(p, alpha);
        } else {
            self.stroke(p, alpha);
            self.fill(p, alpha);
        }
    }

    fn fill(&mut self, p: &usvg::Path, alpha: f32) {
        let Some(f) = p.fill() else { return };
        let Some(rgba) = self.color(f.paint(), alpha * f.opacity().get()) else { return };
        let eo = matches!(f.rule(), usvg::FillRule::EvenOdd);
        self.emit(p.data(), p.abs_transform(), rgba, eo);
    }

    fn stroke(&mut self, p: &usvg::Path, alpha: f32) {
        let Some(s) = p.stroke() else { return };
        let Some(rgba) = self.color(s.paint(), alpha * s.opacity().get()) else { return };
        // Штрих → контур. `resolution_scale` = 1: кривые внутри стыков и скруглений остаются
        // кривыми, разложит их уже растеризатор системы — по тому размеру, которым рисует.
        let Some(outline) = p.data().stroke(&s.to_tiny_skia(), 1.0) else {
            self.skip("обводку не удалось превратить в контур");
            return;
        };
        // Обводка всегда по ненулевому обходу: у контура, который построил штриховальщик,
        // внутренние петли обязаны заливаться, а по чётности они бы выедали дырки.
        //
        // Штрихуем в СВОИХ координатах и переводим уже контур: толщина линии живёт в них же, и
        // штриховать после перевода значило бы штриховать не той толщиной.
        self.emit(&outline, p.abs_transform(), rgba, false);
    }

    /// Цвет с домноженной прозрачностью. `None` — краска, которой формат не знает.
    fn color(&mut self, paint: &Paint, alpha: f32) -> Option<[u8; 4]> {
        let c = match paint {
            Paint::Color(c) => *c,
            Paint::LinearGradient(_) | Paint::RadialGradient(_) => {
                self.skip("градиент — формат хранит только сплошной цвет");
                return None;
            }
            Paint::Pattern(_) => {
                self.skip("узор (pattern)");
                return None;
            }
        };
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a == 0 {
            return None;
        }
        Some([c.red, c.green, c.blue, a])
    }

    /// Выложить контур в `.vg`, переведя его в координаты вьюбокса.
    ///
    /// `ts` — АБСОЛЮТНОЕ преобразование фигуры: то, что накопили группы над ней, плюс то, чем
    /// usvg разворачивает сам `viewBox`. Без него конвертер молча врал на двух очень обычных
    /// случаях: у иконки с `viewBox="0 -960 960 960"` (так выложены Material Symbols) все
    /// координаты отрицательные, и файл выходил пустым на вид; у выгрузки со сдвинутой группой
    /// фигура уезжала на её сдвиг. «Всё это разворачивает usvg» в шапке было правдой лишь
    /// наполовину: развернуть-то он разворачивает, но в преобразование узла, а не в точки.
    fn emit(
        &mut self, path: &tiny_skia_path::Path, ts: tiny_skia_path::Transform, rgba: [u8; 4],
        evenodd: bool,
    ) {
        let Some(path) = path.clone().transform(ts) else {
            self.skip("вырожденное преобразование фигуры (нулевой масштаб?)");
            return;
        };
        let path = &path;
        if self.first_box.is_none() {
            self.first_box = Some(path.bounds());
        }
        self.w.shape(rgba, evenodd);
        for seg in path.segments() {
            match seg {
                PathSegment::MoveTo(p) => self.w.move_to(p.x, p.y),
                PathSegment::LineTo(p) => self.w.line_to(p.x, p.y),
                PathSegment::QuadTo(c, p) => self.w.quad_to(c.x, c.y, p.x, p.y),
                PathSegment::CubicTo(a, b, p) => self.w.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y),
                PathSegment::Close => self.w.close(),
            }
        }
        self.n += 1;
    }
}
