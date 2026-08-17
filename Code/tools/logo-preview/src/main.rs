//! Хостовый предпросмотр знака VOID (Веха 141): `logo-preview [файл.png [ширина высота]]`.
//!
//! Рисует ТЕМ ЖЕ `programs/user/src/logo.rs`, что едет в систему, — файл включён по пути, копии
//! нет. Без этого каждая правка геометрии стоила бы сборки ядра, обновления образа и загрузки
//! QEMU; здесь она стоит секунду.
//!
//! PNG пишется несжатым (deflate «stored»): полсотни строк против крейта ради картинки, которую
//! смотрит один человек.
extern crate alloc;

#[path = "../../../programs/user/src/logo.rs"]
mod logo;

use std::io::Write;

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut c = tag.to_vec();
    c.extend_from_slice(body);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

fn write_png(path: &str, px: &[u8], w: u32, h: u32) {
    let mut raw = Vec::with_capacity((h * (1 + w * 4)) as usize);
    for y in 0..h {
        raw.push(0);
        let o = (y * w * 4) as usize;
        raw.extend_from_slice(&px[o..o + (w * 4) as usize]);
    }
    let mut z = vec![0x78, 0x01];
    for (i, part) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(part.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(part.len() as u16)).to_le_bytes());
        z.extend_from_slice(part);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path).unwrap().write_all(&png).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "preview.png".into());
    let w: u32 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1280);
    let h: u32 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(800);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let t = std::time::Instant::now();
    logo::wallpaper(&mut px, w, h, &logo::Palette::VOID);
    eprintln!("{w}×{h} за {:?}", t.elapsed());
    write_png(&out, &px, w, h);
}
