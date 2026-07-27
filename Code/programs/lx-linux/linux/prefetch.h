/* linux/prefetch.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Подсказки предвыборки кэша — у нас через __builtin_prefetch (или no-op). */
#ifndef _LINUX_PREFETCH_H_SHIM
#define _LINUX_PREFETCH_H_SHIM

static inline void prefetch(const void *x)  { __builtin_prefetch(x, 0); }
static inline void prefetchw(const void *x) { __builtin_prefetch(x, 1); }

#endif /* _LINUX_PREFETCH_H_SHIM */
