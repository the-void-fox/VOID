//! Микробенчи VOID (Веха 28) — цена базовых операций системы глазами userspace.
//!
//! Запускается из `bench_demo` (kmain) с двумя capability: `a0` — store с правами
//! [r w x] (put/get/exec), `a1` — эндпоинт сервера posixfs (для IPC-пинга). Ядро на
//! время сессии глушит трассировку шлюзов (`proc::set_verbose(false)`) — иначе замер
//! мерил бы println, а не syscall.
//!
//! Время — [`void_user::now`] прямо из U-mode (rdtime/rdtsc, не syscall); перевод в
//! наносекунды — [`void_user::TICK_NS`] (на x86 предполагает TSC QEMU TCG ~1 ГГц).
//! ВАЖНО: всё меряется под QEMU (TCG, без KVM) — это цифры ЭМУЛЯЦИИ, они честно
//! сравниваются только с другой системой в том же QEMU (см. README: гость Linux).
#![no_std]
#![no_main]

use void_user::{now, posix, TICK_NS};

/// Десятичная печать числа (форматтера в no_std-бинаре нет — пишем сами).
fn put_num(out: &mut [u8], pos: &mut usize, mut v: usize) {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out[*pos] = tmp[n];
        *pos += 1;
    }
}

fn put_str(out: &mut [u8], pos: &mut usize, s: &str) {
    out[*pos..*pos + s.len()].copy_from_slice(s.as_bytes());
    *pos += s.len();
}

/// Строка отчёта: имя, итерации, всего µs, нс/операция.
fn report(name: &str, iters: usize, ticks: usize) {
    let mut line = [0u8; 160];
    let mut p = 0;
    put_str(&mut line, &mut p, "    ");
    put_str(&mut line, &mut p, name);
    put_str(&mut line, &mut p, ": ");
    put_num(&mut line, &mut p, iters);
    put_str(&mut line, &mut p, " итер · ");
    put_num(&mut line, &mut p, ticks * TICK_NS / 1000);
    put_str(&mut line, &mut p, " µs всего · ~");
    put_num(&mut line, &mut p, ticks * TICK_NS / iters);
    put_str(&mut line, &mut p, " ns/op\n");
    void_user::write(&line[..p]);
}

/// Буфер под замеры кадровых размеров (Веха 129). Статикой, а не кучей: `bench` — no_std-бинарь
/// без `alloc`, и заводить её ради одного буфера незачем.
static mut FRAME_BUF: [u8; 512 * 1024] = [0; 512 * 1024];

/// Отчёт для замеров, где важен не только вызов, но и ПРОПУСКНАЯ СПОСОБНОСТЬ: строка кадра
/// платит за каждый свой байт (BLAKE3 + копия), и «нс на операцию» об этом не говорит ничего.
fn report_bytes(name: &str, iters: usize, size: usize, ticks: usize) {
    let mut line = [0u8; 200];
    let mut p = 0;
    put_str(&mut line, &mut p, "    ");
    put_str(&mut line, &mut p, name);
    put_str(&mut line, &mut p, ": ");
    put_num(&mut line, &mut p, iters);
    put_str(&mut line, &mut p, " итер · ~");
    put_num(&mut line, &mut p, ticks * TICK_NS / iters / 1000);
    put_str(&mut line, &mut p, " µs/op · ");
    // МБ/с = всего байт / всего наносекунд * 1e9 / 1e6; считаем в целых, порядок не теряя.
    let ns = (ticks * TICK_NS).max(1);
    put_num(&mut line, &mut p, iters * size * 1000 / ns);
    put_str(&mut line, &mut p, " МБ/с\n");
    void_user::write(&line[..p]);
}

#[no_mangle]
pub extern "C" fn _start(store_cap: usize, ep: usize) -> ! {
    // 1. Null syscall: SYS_YIELD, других готовых нет — полный круг
    //    trap → диспетчер → enter_user обратно в нас.
    let n = 1000;
    let t0 = now();
    for _ in 0..n {
        void_user::yield_now();
    }
    report("null syscall (yield)", n, now() - t0);

    // 2. IPC-пинг: CALL → posixfs (close несуществующего fd — минимум работы
    //    сервера) → REPLY. Два переключения процессов на круг.
    let n = 300;
    let t0 = now();
    for _ in 0..n {
        posix::close(ep, posix::FD_BASE + 60);
    }
    report("IPC CALL+REPLY", n, now() - t0);

    // 3. Page fault: ленивые страницы кучи — trap, выделение фрейма, map, повтор.
    let pages = 256;
    let base = void_user::heap_map(pages * 4096);
    let t0 = now();
    for i in 0..pages {
        unsafe { core::ptr::write_volatile((base + i * 4096) as *mut u8, 1) };
    }
    report("page fault (ленивая страница)", pages, now() - t0);

    // 4. obj_put: 32-байтные значения, уникальные на каждый прогон (примесь
    //    времени в данных — дедуп store не срезает работу BLAKE3+вставки).
    let n = 100;
    let mut id = [0u8; 32];
    let mut data = [0u8; 32];
    data[8..16].copy_from_slice(&now().to_le_bytes());
    let t0 = now();
    for i in 0..n as u64 {
        data[0..8].copy_from_slice(&i.to_le_bytes());
        void_user::obj_put(store_cap, &data, &mut id);
    }
    report("obj_put 32 Б (BLAKE3+store)", n, now() - t0);

    // 4a. Веха 129 — obj_put НА РАЗМЕРАХ КАДРА. Тридцать два байта меряют накладные расходы
    //     вызова, а окно платит за содержимое: изменившаяся строка окна 630 пикселей — это
    //     2520 байт, а полный кадр такого окна — около двух мегабайт. Через store сегодня ходит
    //     КАЖДЫЙ такой кусок: BLAKE3 по всей длине, выделение в куче ядра, копия, потом уборка.
    //     Это и есть цена, которую снимает разделяемая память, — и чтобы говорить о выигрыше
    //     цифрами, её надо знать до, а не после.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_BUF) };
    buf[8..16].copy_from_slice(&now().to_le_bytes());
    for (label, size, n) in
        [("obj_put строка окна 2.5 КиБ", 2520usize, 200usize), ("obj_put кусок 512 КиБ", 512 * 1024, 10)]
    {
        let t0 = now();
        for i in 0..n as u64 {
            buf[0..8].copy_from_slice(&i.to_le_bytes()); // соль: дедуп не должен срезать работу
            void_user::obj_put(store_cap, &buf[..size], &mut id);
        }
        report_bytes(label, n, size, now() - t0);
    }

    // 5. obj_get последнего значения по content-id.
    let n = 100;
    let mut out = [0u8; 64];
    let t0 = now();
    for _ in 0..n {
        void_user::obj_get(store_cap, &id, &mut out);
    }
    report("obj_get (по content-id)", n, now() - t0);

    // 6. exec: полный жизненный цикл процесса — корень → ELF из store → новое
    //    пространство → загрузка сегментов → запуск → exit → пробуждение родителя.
    let n = 5;
    let t0 = now();
    for _ in 0..n {
        void_user::exec(store_cap, b"bin/hello");
    }
    report("exec bin/hello (полный цикл)", n, now() - t0);

    void_user::exit(0);
}
