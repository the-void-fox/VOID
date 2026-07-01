//! Примитивы синхронизации.
//!
//! Пока одно ядро (один hart), но взаимное исключение уже нужно: глобальный
//! аллокатор обязан быть `Sync`, а значит нужна внутренняя изменяемость под замком.
//! Тот же `SpinLock` пригодится планировщику (Веха 5).
//!
//! Спин-лок НЕ выключает прерывания. Это безопасно, пока обработчики прерываний не
//! берут те же замки (наш таймерный обработчик память не аллоцирует). Когда появится
//! вытеснение/несколько ядер — сделаем вариант, отключающий прерывания на время захвата.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Взаимное исключение через активное ожидание (spin).
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// Безопасно делить между потоками: доступ к data только через захваченный замок.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Захватить замок, крутясь до успеха. Возвращает страж (guard) с доступом к данным.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

/// RAII-страж: пока жив — замок держится; на `drop` — освобождается.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: страж существует только при захваченном замке.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: страж существует только при захваченном замке, и он один.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
