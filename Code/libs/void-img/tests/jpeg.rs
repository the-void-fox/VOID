//! JPEG: сверка с независимым декодером на НАСТОЯЩИХ фотографиях.
//!
//! Байт в байт тут сойтись не обязано, и это не поблажка: стандарт задаёт обратное косинусное
//! преобразование с точностью, а не формулой, и две честные реализации законно расходятся на
//! единицы младшего разряда. Поэтому сверяется не равенство, а РАССТОЯНИЕ: максимум по каналу и
//! среднее. Настоящая ошибка (перепутанные компоненты, сдвиг строк, неверное восстановление
//! цветности) даёт расхождение в десятки, а не в единицы, и через это сито не проходит.
//!
//! Корпус — `IMG/` этого репозитория: снимки с телефона (baseline, прореживание 4:2:0) и
//! пересланные через мессенджер (ПРОГРЕССИВНЫЕ). Второе — ровно та причина, по которой JPEG у
//! нас чужой: своим декодером прогрессивные пришлось бы писать отдельно.

use void_img::{decode, probe, Error, Format};

const MAX: u64 = 64 << 20;

/// Эталон — `jpeg-decoder` от image-rs. Отдаёт RGB (или серое), доводим до RGBA.
fn reference(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let raw = d.decode().expect("эталон не разобрал JPEG");
    let info = d.info().expect("эталон без заголовка");
    let mut px = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            raw.chunks_exact(3).for_each(|c| px.extend_from_slice(&[c[0], c[1], c[2], 0xff]))
        }
        jpeg_decoder::PixelFormat::L8 => {
            raw.iter().for_each(|&g| px.extend_from_slice(&[g, g, g, 0xff]))
        }
        f => panic!("эталон отдал {f:?} — такого в корпусе не ждали"),
    }
    (info.width as u32, info.height as u32, px)
}

/// Файлы корпуса: baseline и прогрессивный.
///
/// Веха 223.11 — корпус теперь ЛЕЖИТ В РЕПОЗИТОРИИ (`tests/corpus/`), и это исправление
/// молчащего теста. Раньше файлы искались в `IMG/` — каталоге со снимками владельца, из которого
/// в репозиторий не попал ни один JPEG. На чужой машине и на CI `corpus()` возвращал пусто, тест
/// печатал «нет корпуса, пропускаю» и рапортовал `ok`. Пять дней он так и значился зелёным,
/// не проверив ни байта: ровно то, о чём предупреждает ci.yml — «молчащий тест хуже
/// отсутствующего, он создаёт уверенность».
///
/// Образцы маленькие (полтора килобайта) и сделаны из `IMG/shots/desktop.png` двумя проходами
/// ImageMagick — `-interlace none` и `-interlace JPEG`. Снимок владельца на мегабайт в
/// репозитории не нужен: тесту важны РЕЖИМЫ кодирования, а не мегапиксели.
///
/// Снимки из `IMG/` добавляются СВЕРХУ, когда они есть: у владельца там живые фотографии с
/// телефона, и если декодер споткнётся о такую, узнать об этом надо. Обязателен же только
/// репозиторный корпус — иначе проверка опять зависела бы от машины.
fn corpus() -> Vec<std::path::PathBuf> {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = vec![
        here.join("tests/corpus/baseline.jpg"),
        here.join("tests/corpus/progressive.jpg"),
    ];
    for p in &out {
        assert!(p.is_file(), "нет образца корпуса {}", p.display());
    }
    if let Ok(entries) = std::fs::read_dir(here.join("../../../IMG")) {
        let mut extra: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "jpg" || e == "jpeg"))
            .collect();
        extra.sort();
        out.extend(extra);
    }
    out
}

#[test]
fn matches_independent_decoder() {
    let files = corpus();
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let (rw, rh, want) = reference(&bytes);
        let got = decode(&bytes, MAX).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!((got.w, got.h), (rw, rh), "{}", path.display());
        assert_eq!(got.px.len(), want.len());

        let (mut worst, mut sum, mut over) = (0u32, 0u64, 0u64);
        for (a, b) in got.px.iter().zip(&want) {
            let d = a.abs_diff(*b) as u32;
            worst = worst.max(d);
            sum += d as u64;
            over += (d > 2) as u64;
        }
        let mean = sum as f64 / want.len() as f64;
        let share = over as f64 * 100.0 / want.len() as f64;
        eprintln!(
            "{}: {rw}×{rh}, среднее {mean:.4}, разошлось больше двух у {share:.4} %, худшее {worst}",
            path.file_name().unwrap().to_string_lossy()
        );
        // Замер на корпусе (2026-08-13): среднее 0.06–0.13, доля «больше двух» около 0.02 %,
        // максимум 5. Пороги стоят с запасом в разы, но на порядок ниже того, что даёт
        // НАСТОЯЩАЯ ошибка: перепутанные компоненты или сдвиг строк — это среднее в десятки.
        assert!(mean < 0.5, "{}: среднее расхождение {mean:.4}", path.display());
        assert!(share < 0.5, "{}: доля крупных расхождений {share:.4} %", path.display());
        assert!(worst <= 8, "{}: канал разошёлся на {worst}", path.display());
    }
}

#[test]
fn probe_and_limits() {
    let files = corpus();
    let Some(path) = files.first() else {
        eprintln!("нет корпуса IMG/, пропускаю");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let p = probe(&bytes).unwrap();
    assert_eq!(p.format, Format::Jpeg);
    assert!(p.w > 0 && p.h > 0);
    // Потолок проверяется ПО ЗАГОЛОВКУ: отказ обязан прийти раньше, чем декодер возьмёт память
    // под мегапиксели. Иначе смысла в потолке нет вовсе.
    let small = p.w as u64 * p.h as u64 - 1;
    assert!(matches!(decode(&bytes, small), Err(Error::TooBig)));
}

/// Порча не должна давать панику: программа обязана либо сказать «файл битый», либо отдать то,
/// что успело прочитаться, — и в обоих случаях остаться живой.
///
/// Обрыв посреди скана zune считает НЕ ошибкой, а концом данных: отдаёт кадр целиком, низ
/// которого не заполнен. Так же ведут себя браузеры, и для обоев это правильнее отказа — но
/// значит, что «декодировалось» не равно «файл целый». Кто хочет гарантий целостности, тот
/// проверяет содержимое по content-id, а не по успеху декодера.
#[test]
fn damage_is_an_error_not_a_panic() {
    let files = corpus();
    let Some(path) = files.first() else {
        eprintln!("нет корпуса IMG/, пропускаю");
        return;
    };
    let bytes = std::fs::read(path).unwrap();

    assert!(matches!(decode(&bytes[..2], MAX), Err(Error::NotImage))); // подписи не хватает

    let cut = decode(&bytes[..bytes.len() / 3], MAX);
    match cut {
        Ok(img) => assert_eq!((img.w, img.h), (probe(&bytes).unwrap().w, probe(&bytes).unwrap().h)),
        Err(e) => eprintln!("обрыв: {e}"),
    }

    let mut head = bytes[..1024].to_vec();
    head.extend_from_slice(&[0x55; 4096]); // мусор вместо данных
    let _ = decode(&head, MAX); // важен не результат, а то, что мы сюда дошли
}
