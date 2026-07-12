//! Контекст выполнения задачи и низкоуровневое переключение между контекстами.
//!
//! «Контекст» здесь — минимум регистров, нужный, чтобы приостановить задачу и позже
//! продолжить её ровно с того же места. Для кооперативного переключения это только
//! callee-saved регистры: `ra` (куда вернуться), `sp` (свой стек) и `s0..s11`.
//! Остальные регистры на момент переключения не важны — их сохранил компилятор вокруг
//! вызова [`context_switch`]. Сама механика сохранения/загрузки — в switch.s.

core::arch::global_asm!(include_str!("switch.s"));

/// Сохранённое состояние задачи. Раскладка строго совпадает с switch.s.
/// Поля не публичны за пределами арха (Веха 24): общий код собирает контексты
/// только конструкторами контракта ниже — как раскладывать regs, знает лишь арх.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Context {
    /// Адрес возврата — куда продолжить исполнение.
    ra: usize,
    /// Указатель стека этой задачи.
    sp: usize,
    /// Callee-saved регистры s0..s11.
    s: [usize; 12],
}

impl Context {
    /// Пустой контекст — для статиков (`RETURN_CTX`) и «заполнится при первом переключении».
    pub const EMPTY: Context = Context { ra: 0, sp: 0, s: [0; 12] };

    /// Контекст новой ядерной ЗАДАЧИ планировщика: первый запуск идёт через
    /// `task_trampoline` (вызовет `entry`, по возврату — `sched::task_exit`).
    pub fn new_task(entry: fn(), sp: usize) -> Context {
        let mut c = Context::EMPTY;
        c.ra = task_trampoline as *const () as usize;
        c.sp = sp;
        c.s[0] = entry as usize; // s0 = адрес функции задачи (см. switch.s/task_trampoline)
        c
    }

    /// Контекст прямого входа в функцию ядра на заданном стеке (лончер сессии процессов).
    pub fn new_kernel(entry: extern "C" fn() -> !, sp: usize) -> Context {
        let mut c = Context::EMPTY;
        c.ra = entry as usize;
        c.sp = sp;
        c
    }
}

extern "C" {
    /// Сохранить текущий контекст в `*old`, загрузить `*new` и продолжить в нём.
    /// Возврат сюда произойдёт, когда кто-то переключится обратно на `old`.
    pub fn context_switch(old: *mut Context, new: *const Context);

    /// Ассемблерная точка первого запуска задачи (кладётся в `ra` в [`Context::new_task`]).
    fn task_trampoline();
}
