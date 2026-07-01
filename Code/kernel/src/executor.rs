//! Веха 9 — **async-executor**: конкурентность поверх объектного пространства.
//!
//! Третий ингредиент стержня из [[0002-persistent-content-addressed-capability-core|ADR 0002]]
//! («Rust + async чинит конкурентность без ретро-заплаток типа io_uring»). В отличие от нитей
//! [[scheduling|`sched`]] (у каждой свой стек, вытеснение по таймеру), async-задача — это
//! **future**: стейт-машина, которую строит компилятор из `async`/`await`. Своего стека нет —
//! состояние живёт в самом future. Executor опрашивает (`poll`) готовые задачи на одном стеке;
//! когда задача не может продвинуться, она возвращает `Pending` и **паркуется**, а разбудит её
//! [`Waker`] (self-wake для кооперативной уступки или чужой `send`, см. [[async-executor]]).
//!
//! Это дополняет, а не заменяет нити: нити вытесняют CPU-задачи, executor даёт дешёвую
//! конкурентность I/O (тысячи задач без тысяч стеков). Пока executor кооперативный.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::sync::SpinLock;

/// Идентификатор async-задачи в executor'е.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TaskId(u64);

/// Async-задача: future, который опрашивается до завершения. `+ Send`, т.к. executor
/// лежит в `static` за [`SpinLock`] (тому нужен `Sync`); наши future захватывают лишь
/// `Arc`/`&'static` — все `Send`.
struct Task {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}

struct Executor {
    tasks: BTreeMap<TaskId, Task>,
    ready: VecDeque<TaskId>,
    next_id: u64,
}

impl Executor {
    const fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            ready: VecDeque::new(),
            next_id: 0,
        }
    }
}

static EXECUTOR: SpinLock<Executor> = SpinLock::new(Executor::new());

/// Waker задачи: разбудить = положить её id обратно в очередь готовых.
struct TaskWaker {
    id: TaskId,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        EXECUTOR.lock().ready.push_back(self.id);
    }
}

/// Поставить async-задачу в очередь. Опрашиваться начнёт в [`run`].
pub fn spawn(future: impl Future<Output = ()> + Send + 'static) -> TaskId {
    let mut ex = EXECUTOR.lock();
    let id = TaskId(ex.next_id);
    ex.next_id += 1;
    ex.tasks.insert(id, Task { future: Box::pin(future) });
    ex.ready.push_back(id);
    id
}

/// Крутить готовые задачи, пока очередь не опустеет (все завершились или спят навсегда).
///
/// Ключевой момент: `poll` идёт ВНЕ замка executor'а. Задачу временно изымаем из карты,
/// опрашиваем, затем возвращаем (если `Pending`). Иначе `wake` из самого `poll` (self-wake
/// или чужой) захотел бы тот же замок → взаимоблокировка.
pub fn run() {
    loop {
        let id = match EXECUTOR.lock().ready.pop_front() {
            Some(id) => id,
            None => return,
        };
        // Задача могла уже завершиться (в очереди остался лишний id) — пропускаем.
        let mut task = match EXECUTOR.lock().tasks.remove(&id) {
            Some(t) => t,
            None => continue,
        };

        let waker = Waker::from(Arc::new(TaskWaker { id }));
        let mut cx = Context::from_waker(&waker);
        match task.future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => { /* готово: задача уже изъята из карты, дропаем */ }
            Poll::Pending => {
                EXECUTOR.lock().tasks.insert(id, task);
            }
        }
    }
}

/// Кооперативно уступить процессор: один `poll` вернёт `Pending` (разбудив себя),
/// следующий — `Ready`. Даёт другим готовым задачам шанс продвинуться.
pub fn yield_now() -> impl Future<Output = ()> {
    struct Yield {
        done: bool,
    }
    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.done {
                Poll::Ready(())
            } else {
                self.done = true;
                cx.waker().wake_by_ref(); // сразу вернуть себя в очередь готовых
                Poll::Pending
            }
        }
    }
    Yield { done: false }
}
