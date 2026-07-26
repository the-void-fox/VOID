/* linux/sched.h — ШИМ lx_emul (Веха 55), НЕ исходник Linux.
 *
 * lib/sort.c зовёт cond_resched() в *_nonatomic-вариантах — точка добровольного вытеснения на
 * длинных сортировках. У нас планировщик вытесняющий (квант таймера), явная уступка не обязательна
 * для корректности — пустышка. Позже, с Lx_kit, здесь появится настоящая эмуляция scheduler'а.
 */
#ifndef _LINUX_SCHED_H_SHIM
#define _LINUX_SCHED_H_SHIM

static inline int cond_resched(void) { return 0; }

#endif /* _LINUX_SCHED_H_SHIM */
