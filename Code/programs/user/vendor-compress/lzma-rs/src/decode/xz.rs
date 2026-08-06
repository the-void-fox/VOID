//! Decoder for the `.xz` file format.

use alloc::format;
use alloc::string::ToString;

use alloc::vec;
use alloc::vec::Vec;
use crate::decode::lzma2::Lzma2Decoder;
use crate::decode::util;
use crate::error;
use crate::xz::crc::{CRC32, CRC64};
use crate::xz::{footer, header, CheckMethod, StreamFlags};
use crate::byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use crate::io;
use crate::io::Read;

#[derive(Debug)]
struct Record {
    unpadded_size: u64,
    unpacked_size: u64,
}

pub fn decode_stream<R, W>(input: &mut R, output: &mut W) -> error::Result<()>
where
    R: io::BufRead,
    W: io::Write,
{
    let header = header::StreamHeader::parse(input)?;

    let mut records: Vec<Record> = vec![];
    let index_size = loop {
        let mut count_input = util::CountBufRead::new(input);
        let header_size = count_input.read_u8()?;
        lzma_info!("XZ block header_size byte: 0x{:02x}", header_size);

        if header_size == 0 {
            lzma_info!("XZ records: {:?}", records);
            check_index(&mut count_input, &records)?;
            let index_size = count_input.count();
            break index_size;
        }

        read_block(
            &mut count_input,
            output,
            header.stream_flags.check_method,
            &mut records,
            header_size,
        )?;
    };

    let crc32 = input.read_u32::<LittleEndian>()?;
    let mut digest = CRC32.digest();
    {
        let mut digested = util::CrcDigestRead::new(input, &mut digest);
        let backward_size = digested.read_u32::<LittleEndian>()?;
        if index_size as u32 != (backward_size + 1) << 2 {
            return Err(error::Error::XzError(format!(
                "Invalid index size: expected {} but got {}",
                (backward_size + 1) << 2,
                index_size
            )));
        }

        let stream_flags = {
            let field = digested.read_u16::<BigEndian>()?;
            StreamFlags::parse(field)?
        };

        if header.stream_flags != stream_flags {
            return Err(error::Error::XzError(format!(
                "Flags in header ({:?}) does not match footer ({:?})",
                header.stream_flags, stream_flags
            )));
        }
    }

    let digest_crc32 = digest.finalize();
    if crc32 != digest_crc32 {
        return Err(error::Error::XzError(format!(
            "Invalid footer CRC32: expected 0x{:08x} but got 0x{:08x}",
            crc32, digest_crc32
        )));
    }

    if !util::read_tag(input, footer::XZ_MAGIC_FOOTER)? {
        return Err(error::Error::XzError(format!(
            "Invalid footer magic, expected {:?}",
            footer::XZ_MAGIC_FOOTER
        )));
    }

    if !util::is_eof(input)? {
        return Err(error::Error::XzError(
            "Unexpected data after last XZ block".to_string(),
        ));
    }
    Ok(())
}

fn check_index<'a, R>(
    count_input: &mut util::CountBufRead<'a, R>,
    records: &[Record],
) -> error::Result<()>
where
    R: io::BufRead,
{
    let mut digest = CRC32.digest();
    let index_tag = 0u8;
    digest.update(&[index_tag]);
    {
        let mut digested = util::CrcDigestRead::new(count_input, &mut digest);

        let num_records = get_multibyte(&mut digested)?;
        if num_records != records.len() as u64 {
            return Err(error::Error::XzError(format!(
                "Expected {} records but got {} records",
                num_records,
                records.len()
            )));
        }

        for (i, record) in records.iter().enumerate() {
            lzma_info!("XZ index checking record {}: {:?}", i, record);

            let unpadded_size = get_multibyte(&mut digested)?;
            if unpadded_size != record.unpadded_size {
                return Err(error::Error::XzError(format!(
                    "Invalid index for record {}: unpadded size ({}) does not match index ({})",
                    i, record.unpadded_size, unpadded_size
                )));
            }

            let unpacked_size = get_multibyte(&mut digested)?;
            if unpacked_size != record.unpacked_size {
                return Err(error::Error::XzError(format!(
                    "Invalid index for record {}: unpacked size ({}) does not match index ({})",
                    i, record.unpacked_size, unpacked_size
                )));
            }
        }
    };
    // TODO: create padding parser function
    let count = count_input.count();
    let padding_size = ((count ^ 0x03) + 1) & 0x03;
    lzma_info!(
        "XZ index: {} byte(s) read, {} byte(s) of padding",
        count,
        padding_size
    );

    {
        let mut digested = util::CrcDigestRead::new(count_input, &mut digest);
        for _ in 0..padding_size {
            let byte = digested.read_u8()?;
            if byte != 0 {
                return Err(error::Error::XzError(
                    "Invalid index padding, must be null bytes".to_string(),
                ));
            }
        }
    };

    let digest_crc32 = digest.finalize();
    lzma_info!("XZ index checking digest 0x{:08x}", digest_crc32);

    let crc32 = count_input.read_u32::<LittleEndian>()?;
    if crc32 != digest_crc32 {
        return Err(error::Error::XzError(format!(
            "Invalid index CRC32: expected 0x{:08x} but got 0x{:08x}",
            crc32, digest_crc32
        )));
    }

    Ok(())
}

#[derive(Debug)]
enum FilterId {
    Lzma2,
}

fn get_filter_id(id: u64) -> error::Result<FilterId> {
    match id {
        0x21 => Ok(FilterId::Lzma2),
        _ => Err(error::Error::XzError(format!("Unknown filter id {}", id))),
    }
}

struct Filter {
    filter_id: FilterId,
    props: Vec<u8>,
}

struct BlockHeader {
    filters: Vec<Filter>,
    packed_size: Option<u64>,
    unpacked_size: Option<u64>,
}

fn read_block<'a, R, W>(
    count_input: &mut util::CountBufRead<'a, R>,
    output: &mut W,
    check_method: CheckMethod,
    records: &mut Vec<Record>,
    header_size: u8,
) -> error::Result<bool>
where
    R: io::BufRead,
    W: io::Write,
{
    let mut digest = CRC32.digest();
    digest.update(&[header_size]);
    let header_size = ((header_size as u64) << 2) - 1;
    // Контрольная сумма блока, посчитанная ПО ХОДУ распаковки (заполняется при одном фильтре).
    let mut block_check: Option<BlockCheck> = None;

    let block_header = {
        let mut taken = count_input.take(header_size);
        let mut digested = io::BufReader::new(util::CrcDigestRead::new(&mut taken, &mut digest));
        read_block_header(&mut digested, header_size)?
    };

    let crc32 = count_input.read_u32::<LittleEndian>()?;
    let digest_crc32 = digest.finalize();
    if crc32 != digest_crc32 {
        return Err(error::Error::XzError(format!(
            "Invalid header CRC32: expected 0x{:08x} but got 0x{:08x}",
            crc32, digest_crc32
        )));
    }

    // ОДИН фильтр (а это все xz, что нам встречаются: `.nar.xz` кэша и индекс канала — чистый
    // LZMA2) распаковывается ПРЯМО В СТОК, без временного буфера на весь блок. Цепочка фильтров
    // по-прежнему идёт через буфер: там выход одного фильтра — вход другого, и потоком это не
    // сделать, не переписав всё вокруг.
    //
    // Цена решения названа честно: байты уходят потребителю ДО того, как сверена контрольная
    // сумма блока. Иначе потоковой распаковки не бывает вообще (так же ведёт себя `xz -dc`).
    // Для VOID это безопасно, потому что ни один потребитель не делает распакованное видимым до
    // конца: `pkg` привязывает корень дерева только после сверки NarHash, а индекс канала — уже
    // после `finish()`; ошибка на любом шаге означает, что корень просто не заведётся, а
    // недостижимые объекты подберёт сборка мусора.
    let filters = block_header.filters;
    let single = filters.len() == 1;

    let mut tmpbuf: Vec<u8> = Vec::new();
    let unpacked_size = if single {
        let mut sink = BlockSink::new(output, check_method);
        let packed_size = decode_filter(count_input, &mut sink, &filters[0])?;
        if let Some(expected_packed_size) = block_header.packed_size {
            if (packed_size as u64) != expected_packed_size {
                return Err(error::Error::XzError(format!(
                    "Invalid compressed size: expected {} but got {}",
                    expected_packed_size, packed_size
                )));
            }
        }
        let (count, check) = sink.done();
        block_check = Some(check);
        count
    } else {
        for (i, filter) in filters.iter().enumerate() {
            if i == 0 {
                // TODO: use SubBufRead on input if packed_size is known?
                let packed_size = decode_filter(count_input, &mut tmpbuf, filter)?;
                if let Some(expected_packed_size) = block_header.packed_size {
                    if (packed_size as u64) != expected_packed_size {
                        return Err(error::Error::XzError(format!(
                            "Invalid compressed size: expected {} but got {}",
                            expected_packed_size, packed_size
                        )));
                    }
                }
            } else {
                let mut newbuf: Vec<u8> = Vec::new();
                decode_filter(
                    &mut io::BufReader::new(tmpbuf.as_slice()),
                    &mut newbuf,
                    filter,
                )?;
                // TODO: does this move or copy?
                tmpbuf = newbuf;
            }
        }
        tmpbuf.len()
    };
    lzma_info!("XZ block decompressed to {} byte(s)", unpacked_size);

    if let Some(expected_unpacked_size) = block_header.unpacked_size {
        if (unpacked_size as u64) != expected_unpacked_size {
            return Err(error::Error::XzError(format!(
                "Invalid decompressed size: expected {} but got {}",
                expected_unpacked_size, unpacked_size
            )));
        }
    }

    let count = count_input.count();
    let padding_size = ((count ^ 0x03) + 1) & 0x03;
    lzma_info!(
        "XZ block: {} byte(s) read, {} byte(s) of padding, check method {:?}",
        count,
        padding_size,
        check_method
    );
    for _ in 0..padding_size {
        let byte = count_input.read_u8()?;
        if byte != 0 {
            return Err(error::Error::XzError(
                "Invalid block padding, must be null bytes".to_string(),
            ));
        }
    }
    match block_check {
        // Потоковый путь: сумма уже посчитана по дороге, буфера с блоком нет.
        Some(check) => validate_block_check_streamed(count_input, check)?,
        None => {
            validate_block_check(count_input, tmpbuf.as_slice(), check_method)?;
            output.write_all(tmpbuf.as_slice())?;
        }
    }
    records.push(Record {
        unpadded_size: (count_input.count() - padding_size) as u64,
        unpacked_size: unpacked_size as u64,
    });

    let finished = false;
    Ok(finished)
}

/// Контрольная сумма блока, досчитанная до конца (VOID, Веха 111).
enum BlockCheck {
    None,
    Crc32(u32),
    Crc64(u64),
    Unsupported,
}

/// Сток блока: отдаёт байты дальше, считает их и ведёт контрольную сумму — всё за один проход,
/// потому что второго прохода по этим байтам не будет: они уже ушли потребителю.
struct BlockSink<'a, W: io::Write> {
    out: &'a mut W,
    count: usize,
    crc32: Option<crate::crc::Digest<'static, u32>>,
    crc64: Option<crate::crc::Digest<'static, u64>>,
    unsupported: bool,
}

impl<'a, W: io::Write> BlockSink<'a, W> {
    fn new(out: &'a mut W, method: CheckMethod) -> Self {
        Self {
            out,
            count: 0,
            crc32: matches!(method, CheckMethod::Crc32).then(|| CRC32.digest()),
            crc64: matches!(method, CheckMethod::Crc64).then(|| CRC64.digest()),
            unsupported: matches!(method, CheckMethod::Sha256),
        }
    }

    fn done(self) -> (usize, BlockCheck) {
        let check = if self.unsupported {
            BlockCheck::Unsupported
        } else if let Some(d) = self.crc32 {
            BlockCheck::Crc32(d.finalize())
        } else if let Some(d) = self.crc64 {
            BlockCheck::Crc64(d.finalize())
        } else {
            BlockCheck::None
        };
        (self.count, check)
    }
}

impl<W: io::Write> io::Write for BlockSink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(d) = self.crc32.as_mut() {
            d.update(buf);
        }
        if let Some(d) = self.crc64.as_mut() {
            d.update(buf);
        }
        self.count += buf.len();
        self.out.write_all(buf)?;
        Ok(buf.len())
    }
}

/// Сверить посчитанную по дороге сумму с полем «Block Check».
fn validate_block_check_streamed<R>(input: &mut R, check: BlockCheck) -> error::Result<()>
where
    R: io::BufRead,
{
    match check {
        BlockCheck::None => Ok(()),
        BlockCheck::Crc32(got) => {
            let want = input.read_u32::<LittleEndian>()?;
            if want != got {
                return Err(error::Error::XzError(format!(
                    "Invalid block CRC32, expected 0x{:08x} but got 0x{:08x}",
                    want, got
                )));
            }
            Ok(())
        }
        BlockCheck::Crc64(got) => {
            let want = input.read_u64::<LittleEndian>()?;
            if want != got {
                return Err(error::Error::XzError(format!(
                    "Invalid block CRC64, expected 0x{:016x} but got 0x{:016x}",
                    want, got
                )));
            }
            Ok(())
        }
        BlockCheck::Unsupported => Err(error::Error::XzError(
            "Unsupported SHA-256 checksum (not yet implemented)".to_string(),
        )),
    }
}

/// Verify block checksum against the "Block Check" field.
///
/// See spec section 3.4 for details.
fn validate_block_check<R>(
    input: &mut R,
    buf: &[u8],
    check_method: CheckMethod,
) -> error::Result<()>
where
    R: io::BufRead,
{
    match check_method {
        CheckMethod::None => (),
        CheckMethod::Crc32 => {
            let crc32 = input.read_u32::<LittleEndian>()?;
            let digest_crc32 = CRC32.checksum(buf);
            if crc32 != digest_crc32 {
                return Err(error::Error::XzError(format!(
                    "Invalid block CRC32, expected 0x{:08x} but got 0x{:08x}",
                    crc32, digest_crc32
                )));
            }
        }
        CheckMethod::Crc64 => {
            let crc64 = input.read_u64::<LittleEndian>()?;
            let digest_crc64 = CRC64.checksum(buf);
            if crc64 != digest_crc64 {
                return Err(error::Error::XzError(format!(
                    "Invalid block CRC64, expected 0x{:016x} but got 0x{:016x}",
                    crc64, digest_crc64
                )));
            }
        }
        // TODO
        CheckMethod::Sha256 => {
            return Err(error::Error::XzError(
                "Unsupported SHA-256 checksum (not yet implemented)".to_string(),
            ));
        }
    }
    Ok(())
}

/// Размер словаря LZMA2 из байта свойств фильтра (xz spec, 5.3.1):
/// биты 0–5 несут показатель, `dict = (2 | (v & 1)) << (v / 2 + 11)`, а `v = 40` — это 4 ГиБ−1.
///
/// Значение только РАСТЁТ с `v`, поэтому непонятное (v > 40) безопаснее трактовать как «держать
/// всё»: лишняя память хуже, чем неверный ответ, но неверный ответ здесь — порча данных.
fn lzma2_dict_size(props: u8) -> usize {
    let v = (props & 0x3F) as u32;
    if v > 40 {
        return usize::MAX;
    }
    if v == 40 {
        return u32::MAX as usize;
    }
    ((2 | (v & 1)) as usize) << (v / 2 + 11)
}

fn decode_filter<R, W>(input: &mut R, output: &mut W, filter: &Filter) -> error::Result<usize>
where
    R: io::BufRead,
    W: io::Write,
{
    let mut count_input = util::CountBufRead::new(input);
    match filter.filter_id {
        FilterId::Lzma2 => {
            if filter.props.len() != 1 {
                return Err(error::Error::XzError(format!(
                    "Invalid properties for filter {:?}",
                    filter.filter_id
                )));
            }
            // Свойство фильтра LZMA2 — один байт с размером СЛОВАРЯ (xz spec, 5.3.1). Апстрим
            // его игнорировал («TODO: properties??»), потому что копил весь вывод в памяти и
            // словарь ему был не нужен. Нам он нужен ровно затем, чтобы НЕ копить: столько
            // байт мы обязаны держать, остальное уходит в сток по дороге (Веха 111).
            Lzma2Decoder::new().decompress_window(
                &mut count_input,
                output,
                lzma2_dict_size(filter.props[0]),
            )?;
            Ok(count_input.count())
        }
    }
}

fn read_block_header<R>(input: &mut R, header_size: u64) -> error::Result<BlockHeader>
where
    R: io::BufRead,
{
    let flags = input.read_u8()?;
    let num_filters = (flags & 0x03) + 1;
    let reserved = flags & 0x3C;
    let has_packed_size = flags & 0x40 != 0;
    let has_unpacked_size = flags & 0x80 != 0;

    lzma_info!(
        "XZ block header: {{ header_size: {}, flags: {}, num_filters: {}, has_packed_size: {}, has_unpacked_size: {} }}",
        header_size,
        flags,
        num_filters,
        has_packed_size,
        has_unpacked_size
    );

    if reserved != 0 {
        return Err(error::Error::XzError(format!(
            "Invalid block flags {}, reserved bits (mask 0x3C) must be zero",
            flags
        )));
    }

    let packed_size = if has_packed_size {
        Some(get_multibyte(input)?)
    } else {
        None
    };

    let unpacked_size = if has_unpacked_size {
        Some(get_multibyte(input)?)
    } else {
        None
    };

    lzma_info!(
        "XZ block header: {{ packed_size: {:?}, unpacked_size: {:?} }}",
        packed_size,
        unpacked_size
    );

    let mut filters: Vec<Filter> = vec![];
    for _ in 0..num_filters {
        let filter_id = get_filter_id(get_multibyte(input)?)?;
        let size_of_properties = get_multibyte(input)?;

        lzma_info!(
            "XZ filter: {{ filter_id: {:?}, size_of_properties: {} }}",
            filter_id,
            size_of_properties
        );

        // Early abort to avoid allocating a large vector
        if size_of_properties > header_size {
            return Err(error::Error::XzError(format!(
                "Size of filter properties exceeds block header size ({} > {})",
                size_of_properties, header_size
            )));
        }

        let mut buf = vec![0; size_of_properties as usize];
        input.read_exact(buf.as_mut_slice()).map_err(|e| {
            error::Error::XzError(format!(
                "Could not read filter properties of size {}: {}",
                size_of_properties, e
            ))
        })?;

        lzma_info!("XZ filter properties: {:?}", buf);

        filters.push(Filter {
            filter_id,
            props: buf,
        })
    }

    if !util::flush_zero_padding(input)? {
        return Err(error::Error::XzError(
            "Invalid block header padding, must be null bytes".to_string(),
        ));
    }

    Ok(BlockHeader {
        filters,
        packed_size,
        unpacked_size,
    })
}

pub fn get_multibyte<R>(input: &mut R) -> error::Result<u64>
where
    R: io::Read,
{
    let mut result = 0;
    for i in 0..9 {
        let byte = input.read_u8()?;
        result ^= ((byte & 0x7F) as u64) << (i * 7);
        if (byte & 0x80) == 0 {
            return Ok(result);
        }
    }

    Err(error::Error::XzError(
        "Invalid multi-byte encoding".to_string(),
    ))
}
