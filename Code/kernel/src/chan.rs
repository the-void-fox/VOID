//! Минимальный async-канал (несколько отправителей, один потребитель) для Вехи 9.
//!
//! Показывает настоящую async-приостановку: `recv().await` возвращает `Pending`, когда
//! очередь пуста, и задача **паркуется** — её [`Waker`] сохраняется в канале. Когда другая
//! задача вызывает [`Sender::send`], она забирает этот waker и будит потребителя. Это то,
//! ради чего нужен waker (в отличие от self-wake в [`crate::executor::yield_now`]): future
//! просыпается по внешнему событию. См. [[async-executor]].

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::sync::SpinLock;

struct Inner<T> {
    queue: VecDeque<T>,
    /// Waker потребителя, если он ждёт значение (единственный потребитель).
    recv_waker: Option<Waker>,
}

/// Конец отправки. Клонируется — производителей может быть несколько.
pub struct Sender<T> {
    inner: Arc<SpinLock<Inner<T>>>,
}

/// Конец приёма (единственный).
pub struct Receiver<T> {
    inner: Arc<SpinLock<Inner<T>>>,
}

/// Создать канал.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(SpinLock::new(Inner {
        queue: VecDeque::new(),
        recv_waker: None,
    }));
    (Sender { inner: inner.clone() }, Receiver { inner })
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender { inner: self.inner.clone() }
    }
}

impl<T> Sender<T> {
    /// Отправить значение и разбудить потребителя, если он припаркован.
    pub fn send(&self, value: T) {
        // Waker будим ПОСЛЕ отпускания замка канала — чтобы не держать два замка сразу.
        let waker = {
            let mut inner = self.inner.lock();
            inner.queue.push_back(value);
            inner.recv_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// Дождаться следующего значения. Если очередь пуста — задача паркуется до `send`.
    pub fn recv(&self) -> Recv<T> {
        Recv { inner: self.inner.clone() }
    }
}

/// Future приёма одного значения.
pub struct Recv<T> {
    inner: Arc<SpinLock<Inner<T>>>,
}

impl<T> Future for Recv<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut inner = self.inner.lock();
        match inner.queue.pop_front() {
            Some(v) => Poll::Ready(v),
            None => {
                // Запомнить, кого будить, когда придёт значение.
                inner.recv_waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
