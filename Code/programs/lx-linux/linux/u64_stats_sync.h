/* linux/u64_stats_sync.h — ШИМ lx_emul (Веха 193), НЕ исходник Linux.
 *
 * В Linux это способ читать 64-битный счётчик на 32-битной машине без рваных значений:
 * писатель помечает начало и конец обновления, читатель повторяет чтение, если попал в
 * середину. У нас обе архитектуры 64-битные, счётчик пишется одной инструкцией, а драйвер
 * работает в одной кооперативной нити — рвать значение попросту нечему.
 *
 * Поэтому всё пусто. Появится SMP внутри драйвера — заглушки перестанут быть заглушками, и
 * искать это место придётся здесь.
 */
#ifndef _LINUX_U64_STATS_SYNC_H_SHIM
#define _LINUX_U64_STATS_SYNC_H_SHIM

#include <linux/types.h>

struct u64_stats_sync { int unused; };

static inline void u64_stats_init(struct u64_stats_sync *s) { (void)s; }
static inline void u64_stats_update_begin(struct u64_stats_sync *s) { (void)s; }
static inline void u64_stats_update_end(struct u64_stats_sync *s) { (void)s; }
static inline unsigned int u64_stats_fetch_begin(const struct u64_stats_sync *s)
{ (void)s; return 0; }
static inline bool u64_stats_fetch_retry(const struct u64_stats_sync *s, unsigned int start)
{ (void)s; (void)start; return false; }
static inline void u64_stats_inc(u64 *p) { (*p)++; }
static inline void u64_stats_add(u64 *p, unsigned long val) { *p += val; }
static inline u64 u64_stats_read(const u64 *p) { return *p; }
static inline void u64_stats_set(u64 *p, u64 val) { *p = val; }

#endif /* _LINUX_U64_STATS_SYNC_H_SHIM */
