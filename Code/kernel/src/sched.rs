//! Кооперативный планировщик задач (round-robin).
//!
//! «Задача» (task) — независимая нить исполнения в ядре: у неё свой стек и свой
//! [`Context`]. Планировщик держит список задач и по вызову [`yield_now`] переключается
//! на следующую готовую (round-robin). Пока планирование **кооперативное**: задача сама
//! решает уступить процессор, вызвав `yield_now`. Вытеснение по таймеру — отдельный шаг
//! (нужно аккуратно управлять состоянием прерываний на каждую задачу).
//!
//! Куча (Веха 4) даёт где хранить стеки и структуры задач; [`SpinLock`] защищает
//! список планировщика.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::context::{context_switch, task_trampoline, Context};
use crate::sync::SpinLock;

/// Размер стека одной задачи (берётся из кучи).
const STACK_SIZE: usize = 32 * 1024;

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Runnable,
    Finished,
}

struct Task {
    name: &'static str,
    context: Context,
    /// Владеем стеком задачи: живёт, пока жива задача; `sp` указывает внутрь него.
    #[allow(dead_code)]
    stack: Vec<u8>,
    state: State,
}

struct Scheduler {
    tasks: Vec<Box<Task>>,
    current: usize,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            current: 0,
        }
    }

    /// Индекс следующей незавершённой задачи после `from` (по кругу).
    /// Если готова только `from` — вернёт её же.
    fn pick_next(&self, from: usize) -> Option<usize> {
        let n = self.tasks.len();
        for i in 1..=n {
            let idx = (from + i) % n;
            if self.tasks[idx].state != State::Finished {
                return Some(idx);
            }
        }
        None
    }
}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());

/// Инициализировать планировщик: сделать текущее исполнение (kmain) задачей «main».
/// Её контекст заполнится автоматически при первом переключении с неё.
pub fn init() {
    let main = Box::new(Task {
        name: "main",
        context: Context::default(),
        stack: Vec::new(), // main работает на загрузочном стеке из linker.ld
        state: State::Runnable,
    });
    SCHED.lock().tasks.push(main);
}

/// Создать задачу с собственным стеком, которая начнёт с функции `entry`.
pub fn spawn(name: &'static str, entry: fn()) {
    let stack = vec![0u8; STACK_SIZE];
    // Стек растёт вниз — начинаем с вершины, выровненной по 16 (требование ABI).
    let sp = align_down(stack.as_ptr() as usize + STACK_SIZE, 16);

    let mut context = Context::default();
    context.ra = task_trampoline as *const () as usize; // куда «вернётся» при старте
    context.sp = sp;
    context.s[0] = entry as usize; // s0 = адрес функции задачи (см. task_trampoline)

    let mut sched = SCHED.lock();
    sched.tasks.push(Box::new(Task {
        name,
        context,
        stack,
        state: State::Runnable,
    }));
}

/// Уступить процессор следующей готовой задаче (round-robin).
pub fn yield_now() {
    let old_ctx: *mut Context;
    let new_ctx: *const Context;
    {
        let mut sched = SCHED.lock();
        let old = sched.current;
        let next = match sched.pick_next(old) {
            Some(n) if n != old => n,
            _ => return, // некому уступить — продолжаем сами
        };
        sched.current = next;
        // Сырые указатели: задачи в Box'ах, их адреса стабильны и переживут drop замка.
        old_ctx = &raw mut sched.tasks[old].context;
        new_ctx = &raw const sched.tasks[next].context;
    } // ВАЖНО: отпускаем замок ДО переключения, иначе следующая задача не сможет его взять

    // SAFETY: контексты валидны и стабильны; замок отпущен.
    unsafe { context_switch(old_ctx, new_ctx) }
}

/// Есть ли ещё незавершённые задачи, кроме текущей.
pub fn other_runnable() -> bool {
    let sched = SCHED.lock();
    let cur = sched.current;
    sched
        .tasks
        .iter()
        .enumerate()
        .any(|(i, t)| i != cur && t.state != State::Finished)
}

/// Имя текущей задачи (для вывода).
pub fn current_name() -> &'static str {
    let sched = SCHED.lock();
    sched.tasks[sched.current].name
}

/// Завершить текущую задачу. Вызывается из task_trampoline, если функция задачи
/// вернулась. Помечаем себя Finished и уступаем навсегда — pick_next нас больше не выберет.
#[no_mangle]
extern "C" fn task_exit() -> ! {
    {
        let mut sched = SCHED.lock();
        let cur = sched.current;
        sched.tasks[cur].state = State::Finished;
    }
    loop {
        yield_now();
    }
}

fn align_down(x: usize, a: usize) -> usize {
    x & !(a - 1)
}
