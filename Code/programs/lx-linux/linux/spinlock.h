/* linux/spinlock.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * Один поток кооперативной модели Lx_kit → блокировки почти no-op (нет вытеснения = нет гонок).
 * Держим счётчик вложенности только для отладки. irqsave-варианты тоже пустые (IRQ ловит
 * отдельная задача на SYS_IRQ_WAIT, а не вложенный обработчик). */
#ifndef _LINUX_SPINLOCK_H_SHIM
#define _LINUX_SPINLOCK_H_SHIM

#include <linux/types.h>

typedef struct { int locked; } spinlock_t;
typedef struct { int locked; } rwlock_t;

#define DEFINE_SPINLOCK(name) spinlock_t name = { 0 }
#define __SPIN_LOCK_UNLOCKED(name) { 0 }

static inline void spin_lock_init(spinlock_t *l)   { l->locked = 0; }
static inline void spin_lock(spinlock_t *l)        { l->locked = 1; }
static inline void spin_unlock(spinlock_t *l)      { l->locked = 0; }
static inline void spin_lock_bh(spinlock_t *l)     { l->locked = 1; }
static inline void spin_unlock_bh(spinlock_t *l)   { l->locked = 0; }
static inline void spin_lock_irq(spinlock_t *l)    { l->locked = 1; }
static inline void spin_unlock_irq(spinlock_t *l)  { l->locked = 0; }
static inline int  spin_trylock(spinlock_t *l)     { l->locked = 1; return 1; }

/* irqsave/irqrestore: flags — фиктивны (прерывания не вкладываем). */
#define spin_lock_irqsave(l, flags)      do { (flags) = 0; spin_lock(l); } while (0)
#define spin_unlock_irqrestore(l, flags) do { (void)(flags); spin_unlock(l); } while (0)

static inline void rwlock_init(rwlock_t *l)        { l->locked = 0; }
#define read_lock(l)   do { } while (0)
#define read_unlock(l) do { } while (0)
#define write_lock(l)  do { } while (0)
#define write_unlock(l) do { } while (0)

#endif /* _LINUX_SPINLOCK_H_SHIM */
