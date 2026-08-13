//! JPEG — целиком чужой (`zune-jpeg`), и это осознанный выбор.
//!
//! Baseline-декодер написать реально: код Хаффмана, деквантование, обратное косинусное
//! преобразование, восстановление цветности и перевод YCbCr→RGB — примерно тысяча строк. Но
//! половина фотографий вокруг (и ровно половина корпуса `IMG/` в этом репозитории) —
//! ПРОГРЕССИВНЫЕ: картинка размазана по нескольким проходам, каждый уточняет либо новые
//! частоты, либо младшие биты уже принятых, и это отдельный декодер поверх первого. Плюс
//! перезапуск потока по маркерам, четыре схемы прореживания цветности и переменное число
//! компонент. Такой объём ради обоев не окупается ничем.
//!
//! Что здесь всё-таки наше: потолок площади проверяется ПО ЗАГОЛОВКУ, до распаковки, и буфер
//! выхода мы выделяем сами через `try_reserve` — чтобы отказ был отказом, а не паникой в чужой
//! куче. Внутренние буферы декодера от нас не зависят, поэтому потолок и важен.

use alloc::vec::Vec;

use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Сразу просим RGBA: перевод цветового пространства декодер делает по дороге, и это дешевле,
/// чем наш проход по готовым пикселям. Альфы у JPEG нет — там будет 255.
fn options() -> DecoderOptions {
    DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        // Размеры в JPEG 16-битные, так что это не ограничение, а снятие внутреннего умолчания
        // (оно у zune скромнее): свой потолок мы считаем в пикселях площади, а не по стороне.
        .set_max_width(u16::MAX as usize)
        .set_max_height(u16::MAX as usize)
}

/// Размеры по заголовку: читаются маркеры до первого скана, пиксели не трогаются.
pub fn info(bytes: &[u8]) -> Result<(u32, u32), crate::Error> {
    let mut d = JpegDecoder::new_with_options(ZCursor::new(bytes), options());
    d.decode_headers().map_err(err)?;
    let i = d.info().ok_or(crate::Error::Header)?;
    Ok((i.width as u32, i.height as u32))
}

/// Распаковать в RGBA8888.
pub fn decode(bytes: &[u8], max_pixels: u64) -> Result<crate::Image, crate::Error> {
    let mut d = JpegDecoder::new_with_options(ZCursor::new(bytes), options());
    d.decode_headers().map_err(err)?;
    let i = d.info().ok_or(crate::Error::Header)?;
    let (w, h) = (i.width as u32, i.height as u32);
    if w == 0 || h == 0 {
        return Err(crate::Error::Header);
    }
    if w as u64 * h as u64 > max_pixels {
        return Err(crate::Error::TooBig);
    }
    let n = d.output_buffer_size().ok_or(crate::Error::Header)?;
    if n != w as usize * h as usize * 4 {
        // Такого быть не должно: цветовое пространство мы задали сами. Если случилось — лучше
        // отказ, чем кадр, в котором строки разъедутся на один байт.
        return Err(crate::Error::Corrupt("декодер отдаёт не RGBA"));
    }
    let mut px: Vec<u8> = Vec::new();
    px.try_reserve_exact(n).map_err(|_| crate::Error::NoMemory)?;
    px.resize(n, 0);
    d.decode_into(&mut px).map_err(err)?;
    Ok(crate::Image { w, h, px })
}

/// Отказы zune сводим к своим. Подробность теряется намеренно: вызывающему важно, файл битый
/// или разновидность не поддержана, а не в каком именно маркере это выяснилось.
fn err(e: zune_jpeg::errors::DecodeErrors) -> crate::Error {
    use zune_jpeg::errors::DecodeErrors as E;
    match e {
        E::Unsupported(_) => crate::Error::Unsupported("такой JPEG zune не берёт"),
        E::LargeDimensions(_) => crate::Error::TooBig,
        E::ExhaustedData | E::IoErrors(_) => crate::Error::Truncated,
        E::IllegalMagicBytes(_) => crate::Error::NotImage,
        E::ZeroError => crate::Error::Header,
        _ => crate::Error::Corrupt("JPEG не по стандарту"),
    }
}
