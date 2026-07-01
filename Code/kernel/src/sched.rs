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
use crate::csr;
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

/// Выполнить `f` с захваченным планировщиком и ВЫКЛЮЧЕННЫМИ прерываниями.
/// Выключать прерывания обязательно: иначе таймер вытеснит нас прямо посреди работы
/// со списком задач, а его обработчик снова полезет в SCHED → взаимоблокировка.
fn with_sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    let sie = csr::irq_save_disable();
    let mut guard = SCHED.lock();
    let r = f(&mut guard);
    drop(guard);
    csr::irq_restore(sie);
    r
}

/// Инициализировать планировщик: сделать текущее исполнение (kmain) задачей «main».
/// Её контекст заполнится автоматически при первом переключении с неё.
pub fn init() {
    let main = Box::new(Task {
        name: "main",
        context: Context::default(),
        stack: Vec::new(), // main работает на загрузочном стеке из linker.ld
        state: State::Runnable,
    });
    with_sched(|s| s.tasks.push(main));
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

    with_sched(|s| {
        s.tasks.push(Box::new(Task {
            name,
            context,
            stack,
            state: State::Runnable,
        }));
    });
}

/// Уступить процессор следующей готовой задаче (round-robin). Вызывается и кооперативно
/// (самой задачей), и из обработчика таймера (вытеснение).
pub fn yield_now() {
    // Выключаем прерывания на всё переключение и запоминаем прежнее состояние SIE ИМЕННО
    // в локальной переменной: она лежит на стеке этой задачи и переживёт переключение,
    // поэтому по возвращении сюда мы восстановим SIE ровно таким, каким он был у НАС.
    // (Кооперативная задача уходила с SIE=1, вытесненная — из trap'а с SIE=0.)
    let sie = csr::irq_save_disable();

    let switch = {
        let mut sched = SCHED.lock();
        let old = sched.current;
        match sched.pick_next(old) {
            Some(next) if next != old => {
                sched.current = next;
                // Сырые указатели: задачи в Box'ах, адреса стабильны.
                let o = &raw mut sched.tasks[old].context;
                let n = &raw const sched.tasks[next].context;
                Some((o, n))
            }
            _ => None, // некому уступить
        }
    };

    if let Some((o, n)) = switch {
        // SAFETY: контексты валидны/стабильны; прерывания выключены на время переключения.
        unsafe { context_switch(o, n) }
    }

    csr::irq_restore(sie);
}

/// Есть ли ещё незавершённые задачи, кроме текущей.
pub fn other_runnable() -> bool {
    with_sched(|s| {
        let cur = s.current;
        s.tasks
            .iter()
            .enumerate()
            .any(|(i, t)| i != cur && t.state != State::Finished)
    })
}

/// Имя текущей задачи (для вывода).
pub fn current_name() -> &'static str {
    with_sched(|s| s.tasks[s.current].name)
}

/// Завершить текущую задачу. Вызывается из task_trampoline, если функция задачи
/// вернулась. Помечаем себя Finished и уступаем навсегда — pick_next нас больше не выберет.
#[no_mangle]
extern "C" fn task_exit() -> ! {
    with_sched(|s| {
        let cur = s.current;
        s.tasks[cur].state = State::Finished;
    });
    loop {
        yield_now();
    }
}

fn align_down(x: usize, a: usize) -> usize {
    x & !(a - 1)
}
