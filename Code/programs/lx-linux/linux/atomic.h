/* linux/atomic.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * В кооперативной один-поток-модели Lx_kit ([[lx-sched]]) весь портированный код сериализован —
 * настоящей конкуренции нет, поэтому atomic_* = обычные операции над полем. Настоящая атомарность
 * нужна ТОЛЬКО на стыке реального IRQ (наш SYS_IRQ_WAIT), там отдельный флаг. */
#ifndef _LINUX_ATOMIC_H_SHIM
#define _LINUX_ATOMIC_H_SHIM

#include <linux/types.h>

typedef struct { int counter; } atomic_t;
typedef struct { long counter; } atomic64_t;

#define ATOMIC_INIT(i) { (i) }

static inline int  atomic_read(const atomic_t *v)         { return v->counter; }
static inline void atomic_set(atomic_t *v, int i)         { v->counter = i; }
static inline void atomic_inc(atomic_t *v)                { v->counter++; }
static inline void atomic_dec(atomic_t *v)                { v->counter--; }
static inline void atomic_add(int i, atomic_t *v)         { v->counter += i; }
static inline void atomic_sub(int i, atomic_t *v)         { v->counter -= i; }
static inline int  atomic_inc_return(atomic_t *v)         { return ++v->counter; }
static inline int  atomic_dec_return(atomic_t *v)         { return --v->counter; }
static inline int  atomic_add_return(int i, atomic_t *v)  { v->counter += i; return v->counter; }
static inline int  atomic_dec_and_test(atomic_t *v)       { return --v->counter == 0; }
static inline int  atomic_inc_and_test(atomic_t *v)       { return ++v->counter == 0; }
static inline int  atomic_sub_and_test(int i, atomic_t *v){ v->counter -= i; return v->counter == 0; }
static inline int  atomic_cmpxchg(atomic_t *v, int old, int new)
{
	int prev = v->counter;
	if (prev == old) v->counter = new;
	return prev;
}
static inline int  atomic_xchg(atomic_t *v, int new)
{
	int prev = v->counter; v->counter = new; return prev;
}

/* Барьеры памяти — на одном ядре без вытеснения не нужны (пустые). */
#define smp_mb()        do { } while (0)
#define smp_rmb()       do { } while (0)
#define smp_wmb()       do { } while (0)
#define smp_mb__before_atomic() do { } while (0)
#define smp_mb__after_atomic()  do { } while (0)
#ifndef barrier
#define barrier()       __asm__ __volatile__("" ::: "memory")
#endif
#define mb()            do { } while (0)
#define rmb()           do { } while (0)
#define wmb()           do { } while (0)
#define dma_wmb()       do { } while (0)
#define dma_rmb()       do { } while (0)

/* Acquire/release — на одном ядре без вытеснения = обычные обращения (+ барьер компилятора). */
#define smp_load_acquire(p)      ({ typeof(*(p)) __v = *(p); barrier(); __v; })
#define smp_store_release(p, v)  do { barrier(); *(p) = (v); } while (0)

#endif /* _LINUX_ATOMIC_H_SHIM */
