//! Вызовы прошивки через SBI (Supervisor Binary Interface).
//!
//! Под нашим ядром (S-mode) работает прошивка OpenSBI (M-mode). Часть операций
//! S-mode не может сделать сам (например, запрограммировать аппаратный таймер без
//! расширения Sstc) и просит прошивку — это и есть SBI-вызов.
//!
//! Механика: кладём аргументы в a0..a5, номер функции (FID) в a6, номер расширения
//! (EID) в a7, выполняем `ecall` (переход в M-mode). Прошивка возвращает пару
//! (error, value) в a0, a1. EID/FID и семантика — из спецификации SBI.

use core::arch::asm;

// EID нужных расширений (это ASCII-аббревиатуры, упакованные в число).
const EID_TIME: usize = 0x5449_4D45; // "TIME" — таймер
#[allow(dead_code)] // понадобится для автотестов (см. shutdown ниже)
const EID_SRST: usize = 0x5352_5354; // "SRST" — system reset

/// Сырой SBI-вызов. Возвращает (error, value) как их вернула прошивка.
#[inline]
fn sbi_call(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> (usize, usize) {
    let err: usize;
    let val: usize;
    unsafe {
        asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inout("a0") a0 => err,
            inout("a1") a1 => val,
            in("a2") a2,
            options(nostack),
        );
    }
    (err, val)
}

/// Запрограммировать следующее таймерное прерывание на абсолютное значение `time`
/// (в тиках таймбазы). Это же действие сбрасывает текущий pending-бит таймера.
/// TIME extension, FID 0: `sbi_set_timer(stime_value)`.
#[inline]
pub fn set_timer(time: u64) {
    sbi_call(EID_TIME, 0, time as usize, 0, 0);
}

/// Корректно выключить машину (в QEMU — завершить процесс).
/// SRST extension, FID 0: `sbi_system_reset(reset_type=0 shutdown, reason=0)`.
/// Пригодится для автотестов, чтобы ядро само завершало QEMU.
#[allow(dead_code)] // ещё не вызывается — задел под автотесты
#[inline]
pub fn shutdown() -> ! {
    sbi_call(EID_SRST, 0, 0, 0, 0);
    // Если по какой-то причине не выключились — просто зависнем.
    loop {
        unsafe { asm!("wfi") }
    }
}
