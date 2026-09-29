//! void-wpa — крипта WPA2-PSK (Веха 217, шаг 4 [[0021-wifi-own-stack]]).
//!
//! Четыре вещи, и все нужны, чтобы доказать точке знание пароля:
//!
//! | что | зачем |
//! |---|---|
//! | SHA-1 | на нём стоит весь WPA2-PSK; ничего современнее стандарт здесь не знает |
//! | HMAC-SHA1 | подпись, из которой собрано всё остальное |
//! | PBKDF2 | пароль + имя сети → парный мастер-ключ (4096 проходов) |
//! | PRF | мастер-ключ + случайные числа сторон → ключи сеанса |
//!
//! ## Почему отдельный крейт
//!
//! Потому что **правильность крипты доказывается только совпадением с эталоном**. Своя реализация
//! SHA-1 либо совпадает с опубликованными векторами побайтно, либо не работает вовсе — третьего
//! нет, и «вроде считает» здесь худший из возможных ответов: рукопожатие просто не сойдётся, а
//! искать причину будут в карте, в драйвере и в точке доступа.
//!
//! Векторы гоняются на ХОСТЕ, поэтому крейт стандалон со своим таргетом — как `vvsh-core`.
//!
//! ## Почему SHA-1 свой, а не крейтом
//!
//! В дереве вендорены `sha2`, `hmac`, `aes` (приехали с TLS-клиентом), а SHA-1 нет: TLS 1.3 его
//! не использует. Тянуть ради восьмидесяти строк ещё один чужой крейт в оффлайн-сборку дороже,
//! чем написать их — тем более что проверяются они теми же векторами, что и чужие.
//!
//! ## Чего здесь НЕТ
//!
//! Шифрования кадров: его делает сама карта (CCMP на борту). Хосту нужно лишь ВЫЧИСЛИТЬ ключи и
//! отдать их карте — этим и ограничен крейт.

#![cfg_attr(not(test), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

// ─── SHA-1 ──────────────────────────────────────────────────────────────────────────────────
//
// FIPS 180-4. Устарел для подписей и сломан для коллизий — но здесь он не подпись, а
// строительный блок, назначенный стандартом 802.11i, и заменить его нечем.

/// Длина свёртки SHA-1 в байтах.
pub const SHA1_LEN: usize = 20;
/// Размер блока SHA-1 — он же размер ключа HMAC после нормализации.
pub const SHA1_BLOCK: usize = 64;

/// Состояние SHA-1 поверх потока байтов.
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; SHA1_BLOCK],
    len: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub const fn new() -> Self {
        Sha1 {
            h: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
            buf: [0; SHA1_BLOCK],
            len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let n = (SHA1_BLOCK - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len == SHA1_BLOCK {
                self.block();
                self.len = 0;
            }
        }
    }

    /// Дописать хвост по правилу набивки и отдать свёртку.
    pub fn finish(mut self) -> [u8; SHA1_LEN] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        // Длина занимает восемь последних байт блока; добиваем нулями до неё.
        while self.len != SHA1_BLOCK - 8 {
            self.update(&[0]);
        }
        self.buf[SHA1_BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        self.block();
        let mut out = [0u8; SHA1_LEN];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn block(&mut self) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                self.buf[i * 4],
                self.buf[i * 4 + 1],
                self.buf[i * 4 + 2],
                self.buf[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for (i, &wi) in w.iter().enumerate() {
            // Четыре четверти по двадцать шагов, у каждой своя функция и своя константа.
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (dst, v) in self.h.iter_mut().zip([a, b, c, d, e]) {
            *dst = dst.wrapping_add(v);
        }
    }
}

/// Свёртка одним вызовом.
pub fn sha1(data: &[u8]) -> [u8; SHA1_LEN] {
    let mut h = Sha1::new();
    h.update(data);
    h.finish()
}

// ─── HMAC-SHA1 ──────────────────────────────────────────────────────────────────────────────

/// Подпись `data` ключом `key` (RFC 2104).
///
/// Ключ длиннее блока сворачивается, короче — добивается нулями. Это часть определения, а не
/// оптимизация: без неё подпись разойдётся с чужой на первом же длинном ключе.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; SHA1_LEN] {
    let mut k = [0u8; SHA1_BLOCK];
    if key.len() > SHA1_BLOCK {
        k[..SHA1_LEN].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; SHA1_BLOCK];
    let mut opad = [0x5cu8; SHA1_BLOCK];
    for i in 0..SHA1_BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finish();
    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finish()
}

// ─── PBKDF2-HMAC-SHA1 ───────────────────────────────────────────────────────────────────────

/// Растянуть пароль в ключ (RFC 2898). Заполняет `out` целиком.
///
/// Медленность здесь — СВОЙСТВО, а не изъян: четыре тысячи проходов стоят человеку доли секунды
/// один раз за подключение, а перебирающему пароли — те же доли секунды на КАЖДЫЙ вариант.
pub fn pbkdf2_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    let mut block = 1u32;
    let mut done = 0usize;
    while done < out.len() {
        // Первый проход считается от соли с номером блока, дальнейшие — от предыдущего ответа.
        let mut seed = [0u8; 64];
        let n = salt.len().min(seed.len() - 4);
        seed[..n].copy_from_slice(&salt[..n]);
        seed[n..n + 4].copy_from_slice(&block.to_be_bytes());
        let mut u = hmac_sha1(password, &seed[..n + 4]);
        let mut acc = u;
        for _ in 1..rounds {
            u = hmac_sha1(password, &u);
            for (a, b) in acc.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        let take = (out.len() - done).min(SHA1_LEN);
        out[done..done + take].copy_from_slice(&acc[..take]);
        done += take;
        block += 1;
    }
}

/// Парный мастер-ключ сети: пароль и ИМЯ СЕТИ, 4096 проходов, 32 байта.
///
/// Имя сети здесь работает солью — поэтому один и тот же пароль в двух разных сетях даёт разные
/// ключи, и таблицу заранее посчитать можно только под конкретное имя.
pub fn pmk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2_sha1(passphrase, ssid, 4096, &mut out);
    out
}

// ─── PRF ────────────────────────────────────────────────────────────────────────────────────

/// Псевдослучайная функция 802.11i: из ключа, надписи и данных — сколько угодно байт.
///
/// Устроена просто: подписываем `надпись || 0 || данные || номер` и склеиваем ответы, пока не
/// наберётся нужная длина. Проверять её отдельными векторами незачем — она тонкий слой над
/// [`hmac_sha1`], который сверен с RFC 2202; настоящая её проверка это сошедшееся рукопожатие.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mut i = 0u8;
    let mut done = 0usize;
    while done < out.len() {
        let mut m = [0u8; 128];
        let mut n = 0;
        for src in [label, &[0u8][..], data, &[i][..]] {
            let take = src.len().min(m.len() - n);
            m[n..n + take].copy_from_slice(&src[..take]);
            n += take;
        }
        let d = hmac_sha1(key, &m[..n]);
        let take = (out.len() - done).min(SHA1_LEN);
        out[done..done + take].copy_from_slice(&d[..take]);
        done += take;
        i += 1;
    }
}

/// Парный временный ключ сеанса: 48 байт, из которых карте нужны последние шестнадцать.
///
/// Порядок склейки задан стандартом и НЕ произволен: адреса и случайные числа идут от меньшего
/// к большему, чтобы обе стороны, считая независимо, получили одно и то же. Перепутать порядок —
/// значит получить разные ключи у станции и точки и не понять, почему подпись не сходится.
pub fn ptk(pmk: &[u8; 32], mac_a: &[u8; 6], mac_b: &[u8; 6], nonce_a: &[u8; 32], nonce_b: &[u8; 32]) -> [u8; 48] {
    let (m1, m2) = if mac_a <= mac_b { (mac_a, mac_b) } else { (mac_b, mac_a) };
    let (n1, n2) = if nonce_a <= nonce_b { (nonce_a, nonce_b) } else { (nonce_b, nonce_a) };
    let mut data = [0u8; 76];
    data[..6].copy_from_slice(m1);
    data[6..12].copy_from_slice(m2);
    data[12..44].copy_from_slice(n1);
    data[44..76].copy_from_slice(n2);
    let mut out = [0u8; 48];
    prf(pmk, b"Pairwise key expansion", &data, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // ── SHA-1: векторы FIPS 180-2 ────────────────────────────────────────────

    #[test]
    fn sha1_совпадает_с_эталоном() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    /// Миллион букв «a» — вектор, который ловит ошибки в СЧЁТЧИКЕ ДЛИНЫ и в набивке: на коротких
    /// входах они не проявляются вовсе.
    #[test]
    fn sha1_на_миллионе_байт() {
        let mut h = Sha1::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(hex(&h.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    /// Порционная подача обязана дать то же, что и одним куском: иначе поток и одиночный вызов
    /// разойдутся, а поймается это на кадре неудачной длины.
    #[test]
    fn порции_не_меняют_свёртку() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let целиком = sha1(&data);
        for шаг in [1, 3, 64, 63, 65, 127] {
            let mut h = Sha1::new();
            for кусок in data.chunks(шаг) {
                h.update(кусок);
            }
            assert_eq!(h.finish(), целиком, "порции по {шаг} дали другую свёртку");
        }
    }

    // ── HMAC-SHA1: векторы RFC 2202 ──────────────────────────────────────────

    #[test]
    fn hmac_совпадает_с_эталоном() {
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert_eq!(
            hex(&hmac_sha1(&[0xaa; 20], &[0xdd; 50])),
            "125d7342b9ac11cd91a39af48aa17b4f63f175d3"
        );
    }

    /// Ключ ДЛИННЕЕ блока сворачивается — это часть определения, а не оптимизация. Пароль Wi-Fi
    /// короткий, но PRF подписывает ключами всех длин, и ошибка здесь тихая.
    #[test]
    fn длинный_ключ_сворачивается() {
        assert_eq!(
            hex(&hmac_sha1(&[0xaa; 80], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "aa4ae5e15272d00e95705637ce8a3b55ed402112"
        );
    }

    // ── PBKDF2: векторы RFC 6070 ─────────────────────────────────────────────

    #[test]
    fn pbkdf2_совпадает_с_эталоном() {
        let mut o = [0u8; 20];
        pbkdf2_sha1(b"password", b"salt", 1, &mut o);
        assert_eq!(hex(&o), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
        pbkdf2_sha1(b"password", b"salt", 2, &mut o);
        assert_eq!(hex(&o), "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957");
        pbkdf2_sha1(b"password", b"salt", 4096, &mut o);
        assert_eq!(hex(&o), "4b007901b765489abead49d926f721d065a429c1");
    }

    /// Вывод длиннее одной свёртки: блоки склеиваются, и номер блока обязан входить в счёт.
    /// Ключ сети — ровно этот случай (32 байта из двадцатибайтных кусков).
    #[test]
    fn pbkdf2_склеивает_блоки() {
        let mut o = [0u8; 25];
        pbkdf2_sha1(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, &mut o);
        assert_eq!(hex(&o), "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038");
    }

    // ── ключ сети: векторы IEEE 802.11i ──────────────────────────────────────

    /// Эталон из стандарта: пароль `password`, сеть `IEEE`. Это и есть главная проверка крейта —
    /// если она сходится, значит пароль превращается в ключ ровно так же, как у всех остальных.
    #[test]
    fn ключ_сети_совпадает_с_эталоном() {
        assert_eq!(
            hex(&pmk(b"password", b"IEEE")),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
    }

    #[test]
    fn имя_сети_меняет_ключ() {
        assert_ne!(pmk(b"password", b"IEEE"), pmk(b"password", b"OTHER"));
        assert_ne!(pmk(b"password", b"IEEE"), pmk(b"Password", b"IEEE"));
    }

    // ── ключи сеанса ─────────────────────────────────────────────────────────

    /// Обе стороны считают ключ НЕЗАВИСИМО и обязаны получить одно и то же — порядок склейки для
    /// того и задан «от меньшего к большему». Проверяем перестановкой аргументов: если бы порядок
    /// зависел от того, кто считает, станция и точка разошлись бы и подпись не сошлась.
    #[test]
    fn ключ_сеанса_не_зависит_от_того_кто_считает() {
        let k = pmk(b"password", b"IEEE");
        let (a, b) = ([0x02u8; 6], [0x0au8; 6]);
        let (na, nb) = ([0x11u8; 32], [0x22u8; 32]);
        assert_eq!(ptk(&k, &a, &b, &na, &nb), ptk(&k, &b, &a, &nb, &na));
    }

    #[test]
    fn ключ_сеанса_меняется_от_любого_входа() {
        let k = pmk(b"password", b"IEEE");
        let (a, b) = ([0x02u8; 6], [0x0au8; 6]);
        let (na, nb) = ([0x11u8; 32], [0x22u8; 32]);
        let базовый = ptk(&k, &a, &b, &na, &nb);
        assert_ne!(базовый, ptk(&pmk(b"other", b"IEEE"), &a, &b, &na, &nb));
        assert_ne!(базовый, ptk(&k, &[0x03; 6], &b, &na, &nb));
        assert_ne!(базовый, ptk(&k, &a, &b, &[0x12; 32], &nb));
    }


    // ── распаковка группового ключа: вектор RFC 3394 ─────────────────────────

    #[test]
    fn распаковка_ключа_совпадает_с_эталоном() {
        let kek: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let wrapped: [u8; 24] = [
            0x1f, 0xa6, 0x8b, 0x0a, 0x81, 0x12, 0xb4, 0x47, 0xae, 0xf3, 0x4b, 0xd8, 0xfb, 0x5a,
            0x7b, 0x82, 0x9d, 0x3e, 0x86, 0x23, 0x71, 0xd2, 0xcf, 0xe5,
        ];
        let mut out = [0u8; 16];
        assert!(aes_unwrap(&kek, &wrapped, &mut out), "контрольное слово не сошлось");
        assert_eq!(hex(&out), "00112233445566778899aabbccddeeff");
    }

    /// Чужой ключ обязан ОТКАЗАТЬ, а не отдать мусор. Именно здесь виден неверный пароль:
    /// ключ распаковки выведен из него.
    #[test]
    fn чужим_ключом_распаковка_отказывает() {
        let wrapped: [u8; 24] = [
            0x1f, 0xa6, 0x8b, 0x0a, 0x81, 0x12, 0xb4, 0x47, 0xae, 0xf3, 0x4b, 0xd8, 0xfb, 0x5a,
            0x7b, 0x82, 0x9d, 0x3e, 0x86, 0x23, 0x71, 0xd2, 0xcf, 0xe5,
        ];
        let mut out = [0u8; 16];
        assert!(!aes_unwrap(&[0xff; 16], &wrapped, &mut out), "чужой ключ приняли за свой");
        // И на кривых длинах — отказ, а не паника: кадр приезжает из эфира.
        assert!(!aes_unwrap(&[0; 16], &wrapped[..15], &mut out));
        assert!(!aes_unwrap(&[0; 16], &[], &mut out));
    }

    // ── кадр рукопожатия ─────────────────────────────────────────────────────

    /// Собрали — разобрали: поля обязаны вернуться теми же. Разбор и сборка пишутся по одному
    /// описанию, и разойтись им значит не суметь поговорить с точкой.
    #[test]
    fn кадр_собирается_и_разбирается_обратно() {
        let kck = [0x11u8; 16];
        let replay = [0, 0, 0, 0, 0, 0, 0, 7];
        let nonce = [0x33u8; 32];
        let данные = [48u8, 2, 1, 0];
        let f = build_key_frame(KI_VERSION_2 | KI_PAIRWISE | KI_MIC, &replay, &nonce, &kck, &данные);
        let kf = parse_key_frame(&f).expect("свой же кадр не разобрался");
        assert_eq!(kf.info, KI_VERSION_2 | KI_PAIRWISE | KI_MIC);
        assert_eq!(kf.replay, replay);
        assert_eq!(kf.nonce, nonce);
        assert_eq!(kf.data, &данные);
        assert!(key_mic_ok(&kck, &f, &kf.mic), "своя же подпись не сошлась");
    }

    /// Подпись считается по кадру с ОБНУЛЁННЫМ полем подписи — иначе она подписывала бы саму
    /// себя и не сошлась бы ни у кого. Проверяем тем, что чужой ключ её ломает.
    #[test]
    fn подпись_ловит_чужой_ключ() {
        let replay = [0u8; 8];
        let nonce = [0x44u8; 32];
        let f = build_key_frame(KI_VERSION_2 | KI_PAIRWISE | KI_MIC, &replay, &nonce, &[0x11; 16], &[]);
        let kf = parse_key_frame(&f).unwrap();
        assert!(key_mic_ok(&[0x11; 16], &f, &kf.mic));
        assert!(!key_mic_ok(&[0x22; 16], &f, &kf.mic), "подпись сошлась с чужим ключом");
    }

    /// Кадр из эфира может быть каким угодно. Разбор обязан отказать, а не упасть.
    #[test]
    fn обрезанный_кадр_разбор_не_роняет() {
        let f = build_key_frame(KI_VERSION_2 | KI_PAIRWISE, &[0; 8], &[0; 32], &[], &[1, 2, 3]);
        for обрез in 0..f.len() {
            let _ = parse_key_frame(&f[..обрез]); // важно лишь, что не паникует
        }
        // Длина вложения больше самого кадра — враньё, и его надо заметить.
        let mut порченый = f.clone();
        порченый[EAPOL_HDR + KEY_DATA_LEN] = 0xff;
        assert!(parse_key_frame(&порченый).is_none(), "поверили длине вложения, а не кадру");
    }

    #[test]
    fn групповой_ключ_находится_во_вложении() {
        // Вложение: dd, длина, 00-0f-ac, вид 1, номер ключа, запас, сам ключ.
        let mut kde = alloc::vec![0xdd, 6 + 16, 0x00, 0x0f, 0xac, 0x01, 0x02, 0x00];
        kde.extend_from_slice(&[0x5a; 16]);
        let (номер, ключ) = find_gtk(&kde).expect("ключ не нашёлся");
        assert_eq!(номер, 2);
        assert_eq!(ключ, &[0x5a; 16]);
        // Вложение другого вида — не ключ.
        assert!(find_gtk(&[0xdd, 4, 0x00, 0x0f, 0xac, 0x04]).is_none());
        assert!(find_gtk(&[]).is_none());
    }

    #[test]
    fn prf_выдаёт_сколько_просят() {
        for n in [1usize, 16, 20, 21, 48, 64] {
            let mut o = vec![0u8; n];
            prf(&[0x0b; 20], b"prefix", b"data", &mut o);
            assert_eq!(o.len(), n);
            assert!(o.iter().any(|&b| b != 0), "{n} байт вышли нулями");
        }
    }
}

// ─── распаковка группового ключа (RFC 3394) ─────────────────────────────────────────────────

/// Развернуть ключ, завёрнутый по AES Key Wrap. `false` — контрольное слово не сошлось.
///
/// Контрольное слово здесь и есть проверка: развернув чужим ключом, получишь мусор, и первые
/// восемь байт не совпадут с ожидаемыми. Поэтому неверный пароль виден ЗДЕСЬ, а не «потом где-то
/// в сети».
pub fn aes_unwrap(kek: &[u8], wrapped: &[u8], out: &mut [u8]) -> bool {
    use aes::cipher::{BlockDecrypt, KeyInit};
    if wrapped.len() < 16 || wrapped.len() % 8 != 0 || out.len() + 8 != wrapped.len() {
        return false;
    }
    let Ok(cipher) = aes::Aes128::new_from_slice(kek) else { return false };
    let n = out.len() / 8;
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    out.copy_from_slice(&wrapped[8..]);
    // Шесть проходов в обратном порядке — так определён алгоритм.
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let t = (n * j + i) as u64;
            for (b, x) in a.iter_mut().zip(t.to_be_bytes()) {
                *b ^= x;
            }
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&out[(i - 1) * 8..i * 8]);
            cipher.decrypt_block((&mut block).into());
            a.copy_from_slice(&block[..8]);
            out[(i - 1) * 8..i * 8].copy_from_slice(&block[8..]);
        }
    }
    a == [0xa6; 8]
}

// ─── кадр EAPOL-Key ─────────────────────────────────────────────────────────────────────────
//
// Рукопожатие едет обычными кадрами с данными, внутри которых лежит EAPOL. Разбор и сборка —
// чистая работа с байтами, поэтому она здесь, рядом с криптой, и проверяется на хосте.

/// Смещения внутри тела EAPOL-Key (после трёх байт заголовка EAPOL).
pub const KEY_INFO: usize = 1;
pub const KEY_REPLAY: usize = 5;
pub const KEY_NONCE: usize = 13;
pub const KEY_MIC: usize = 77;
pub const KEY_DATA_LEN: usize = 93;
/// Длина тела до данных. Дальше — длина данных и сами данные.
pub const KEY_BODY: usize = 95;
/// Заголовок EAPOL: версия, тип, длина.
pub const EAPOL_HDR: usize = 4;

/// Биты поля «сведения о ключе».
pub const KI_PAIRWISE: u16 = 1 << 3;
pub const KI_INSTALL: u16 = 1 << 6;
pub const KI_ACK: u16 = 1 << 7;
pub const KI_MIC: u16 = 1 << 8;
pub const KI_SECURE: u16 = 1 << 9;
pub const KI_ENCRYPTED: u16 = 1 << 12;
/// Вид описания ключа: 2 — подпись HMAC-SHA1, групповой ключ завёрнут по AES.
pub const KI_VERSION_2: u16 = 2;

/// Что удалось понять из кадра рукопожатия.
pub struct KeyFrame<'a> {
    pub info: u16,
    pub replay: [u8; 8],
    pub nonce: [u8; 32],
    pub mic: [u8; 16],
    pub data: &'a [u8],
}

/// Разобрать кадр EAPOL-Key. `None` — это не он либо он обрезан.
pub fn parse_key_frame(f: &[u8]) -> Option<KeyFrame<'_>> {
    if f.len() < EAPOL_HDR + KEY_BODY || f[1] != 3 {
        return None; // тип 3 — EAPOL-Key; всё прочее нам не адресовано
    }
    let b = &f[EAPOL_HDR..];
    let dlen = u16::from_be_bytes([b[KEY_DATA_LEN], b[KEY_DATA_LEN + 1]]) as usize;
    if b.len() < KEY_BODY + dlen {
        return None;
    }
    let mut kf = KeyFrame {
        info: u16::from_be_bytes([b[KEY_INFO], b[KEY_INFO + 1]]),
        replay: [0; 8],
        nonce: [0; 32],
        mic: [0; 16],
        data: &b[KEY_BODY..KEY_BODY + dlen],
    };
    kf.replay.copy_from_slice(&b[KEY_REPLAY..KEY_REPLAY + 8]);
    kf.nonce.copy_from_slice(&b[KEY_NONCE..KEY_NONCE + 32]);
    kf.mic.copy_from_slice(&b[KEY_MIC..KEY_MIC + 16]);
    Some(kf)
}

/// Подпись кадра рукопожатия ключом `kck` (первые 16 байт ключа сеанса).
///
/// Считается по ВСЕМУ кадру EAPOL с обнулённым полем подписи — иначе подпись подписывала бы саму
/// себя. Берутся первые 16 байт свёртки.
pub fn key_mic(kck: &[u8], frame: &[u8]) -> [u8; 16] {
    let mut tmp = [0u8; 256];
    let n = frame.len().min(tmp.len());
    tmp[..n].copy_from_slice(&frame[..n]);
    let at = EAPOL_HDR + KEY_MIC;
    if at + 16 <= n {
        tmp[at..at + 16].fill(0);
    }
    let d = hmac_sha1(kck, &tmp[..n]);
    let mut mic = [0u8; 16];
    mic.copy_from_slice(&d[..16]);
    mic
}

/// Сошлась ли подпись кадра. Именно здесь ловится НЕВЕРНЫЙ ПАРОЛЬ: ключ сеанса выведен из него,
/// и при чужом пароле подпись точки не сойдётся с нашей.
pub fn key_mic_ok(kck: &[u8], frame: &[u8], mic: &[u8; 16]) -> bool {
    key_mic(kck, frame) == *mic
}

/// Собрать наш кадр рукопожатия. `data` — вложение (наше описание защиты либо пусто).
/// Подпись ставится последней, поверх уже собранного кадра.
#[cfg(feature = "alloc")]
pub fn build_key_frame(
    info: u16,
    replay: &[u8; 8],
    nonce: &[u8; 32],
    kck: &[u8],
    data: &[u8],
) -> alloc::vec::Vec<u8> {
    let mut f = alloc::vec::Vec::with_capacity(EAPOL_HDR + KEY_BODY + data.len());
    let body = KEY_BODY + data.len();
    f.extend_from_slice(&[2, 3]); // версия EAPOL, тип «ключ»
    f.extend_from_slice(&(body as u16).to_be_bytes());
    f.push(2); // вид описания: RSN
    f.extend_from_slice(&info.to_be_bytes());
    f.extend_from_slice(&16u16.to_be_bytes()); // длина ключа
    f.extend_from_slice(replay);
    f.extend_from_slice(nonce);
    f.extend_from_slice(&[0u8; 16]); // вектор
    f.extend_from_slice(&[0u8; 8]); // счётчик приёма
    f.extend_from_slice(&[0u8; 8]); // запас
    f.extend_from_slice(&[0u8; 16]); // место под подпись
    f.extend_from_slice(&(data.len() as u16).to_be_bytes());
    f.extend_from_slice(data);
    if info & KI_MIC != 0 {
        let mic = key_mic(kck, &f);
        let at = EAPOL_HDR + KEY_MIC;
        f[at..at + 16].copy_from_slice(&mic);
    }
    f
}

/// Найти групповой ключ во вложении третьего кадра (вложения вида `dd <длина> 00-0f-ac 01 …`).
/// `None` — вложения с ключом нет.
pub fn find_gtk(data: &[u8]) -> Option<(u8, &[u8])> {
    let mut i = 0;
    while i + 2 <= data.len() {
        let len = data[i + 1] as usize;
        if data[i] != 0xdd || i + 2 + len > data.len() {
            i += 2 + len;
            continue;
        }
        let kde = &data[i + 2..i + 2 + len];
        // Код 00-0f-ac и вид 1 — это групповой ключ; дальше байт с его номером и сам ключ.
        if kde.len() >= 6 && kde[..4] == [0x00, 0x0f, 0xac, 0x01] {
            return Some((kde[4] & 0x03, &kde[6..]));
        }
        i += 2 + len;
    }
    None
}
