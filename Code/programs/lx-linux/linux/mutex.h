/* linux/mutex.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * Мьютекс. В кооперативной один-поток-модели Lx_kit ([[lx-sched]]) сон под мьютексом невозможен
 * посреди критической секции без явного yield, а гонок нет → mutex почти no-op. Держим владельца
 * для отладки. Настоящее ожидание (если понадобится) сведём на wait_event (Веха 64). */
#ifndef _LINUX_MUTEX_H_SHIM
#define _LINUX_MUTEX_H_SHIM

#include <linux/types.h>

struct mutex { int locked; };

#define DEFINE_MUTEX(name) struct mutex name = { 0 }
#define __MUTEX_INITIALIZER(name) { 0 }

static inline void mutex_init(struct mutex *m)    { m->locked = 0; }
static inline void mutex_lock(struct mutex *m)    { m->locked = 1; }
static inline void mutex_unlock(struct mutex *m)  { m->locked = 0; }
static inline int  mutex_trylock(struct mutex *m) { m->locked = 1; return 1; }
static inline int  mutex_is_locked(struct mutex *m) { return m->locked; }

#endif /* _LINUX_MUTEX_H_SHIM */
