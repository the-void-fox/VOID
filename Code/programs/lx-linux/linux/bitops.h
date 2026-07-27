/* linux/bitops.h — ШИМ lx_emul (Веха 58), НЕ исходник Linux.
 *
 * Битовые операции — вездесущи в драйверах (флаги состояния, битовые карты очередей/векторов,
 * маски регистров, hweight). Даём привычный набор: BIT/GENMASK/BITS_*, set_bit/clear_bit/test_bit
 * (+ test_and_*), find_first_bit/find_next_bit + for_each_set_bit, ffs/fls/__ffs, hweight*. Софтовый
 * popcount hweight маршрутизируем в РЕАЛЬНЫЙ lib/hweight.c ядра (__sw_hweight*). Растёт по надобности.
 *
 * ВАЖНО: set_bit/clear_bit тут НЕ атомарны (одно ядро, драйвер-харнесс однопоточный). Под
 * конкурентный IRQ-поток (нити lx_emul) позже понадобятся настоящие атомики — тогда допишем.
 */
#ifndef _LINUX_BITOPS_H_SHIM
#define _LINUX_BITOPS_H_SHIM

#include <asm/types.h>
#include <linux/types.h>
#include <strings.h> /* newlib: ffs/ffsl/ffsll — та же семантика, что у ядра (не переопределяем) */

#define BITS_PER_LONG      64 /* обе арх VOID 64-битные (-DCONFIG_64BIT) */
#define BITS_PER_LONG_LONG 64
#define BITS_PER_BYTE      8

#define BIT(nr)          (1UL << (nr))
#define BIT_ULL(nr)      (1ULL << (nr))
#define BIT_MASK(nr)     (1UL << ((nr) % BITS_PER_LONG))
#define BIT_WORD(nr)     ((nr) / BITS_PER_LONG)
#define BITS_TO_LONGS(n) (((n) + BITS_PER_LONG - 1) / BITS_PER_LONG)

/* Непрерывная маска бит [l..h] (как ядровый GENMASK). */
#define GENMASK(h, l)     (((~0UL) << (l)) & (~0UL >> (BITS_PER_LONG - 1 - (h))))
#define GENMASK_ULL(h, l) (((~0ULL) << (l)) & (~0ULL >> (BITS_PER_LONG_LONG - 1 - (h))))

#define DECLARE_BITMAP(name, bits) unsigned long name[BITS_TO_LONGS(bits)]

static inline void set_bit(unsigned long nr, volatile unsigned long *addr)
{
	addr[BIT_WORD(nr)] |= BIT_MASK(nr);
}
static inline void clear_bit(unsigned long nr, volatile unsigned long *addr)
{
	addr[BIT_WORD(nr)] &= ~BIT_MASK(nr);
}
static inline void change_bit(unsigned long nr, volatile unsigned long *addr)
{
	addr[BIT_WORD(nr)] ^= BIT_MASK(nr);
}
static inline int test_bit(unsigned long nr, const volatile unsigned long *addr)
{
	return (addr[BIT_WORD(nr)] >> (nr % BITS_PER_LONG)) & 1UL;
}
static inline int test_and_set_bit(unsigned long nr, volatile unsigned long *addr)
{
	unsigned long mask = BIT_MASK(nr);
	volatile unsigned long *p = addr + BIT_WORD(nr);
	int old = (*p & mask) != 0;
	*p |= mask;
	return old;
}
static inline int test_and_clear_bit(unsigned long nr, volatile unsigned long *addr)
{
	unsigned long mask = BIT_MASK(nr);
	volatile unsigned long *p = addr + BIT_WORD(nr);
	int old = (*p & mask) != 0;
	*p &= ~mask;
	return old;
}
/* Неатомарные варианты — у нас те же операции (см. оговорку выше). */
#define __set_bit(nr, addr)          set_bit((nr), (addr))
#define __clear_bit(nr, addr)        clear_bit((nr), (addr))
#define __test_and_set_bit(nr, addr) test_and_set_bit((nr), (addr))

static inline unsigned long __ffs(unsigned long word) { return __builtin_ctzl(word); }
static inline unsigned long __fls(unsigned long word) { return BITS_PER_LONG - 1 - __builtin_clzl(word); }
/* ffs/fls — из newlib <strings.h> (та же семантика 1-based MSB/LSB). fls64 — ядровый, даём сами. */
static inline int fls64(u64 x) { return x ? (64 - __builtin_clzll(x)) : 0; }

/* Простой (по-битовый) поиск — корректно; ядро оптимизирует по словам, нам довольно правильности. */
static inline unsigned long find_next_bit(const unsigned long *addr,
					  unsigned long size, unsigned long offset)
{
	unsigned long i;
	for (i = offset; i < size; i++)
		if (addr[BIT_WORD(i)] & BIT_MASK(i))
			return i;
	return size;
}
static inline unsigned long find_first_bit(const unsigned long *addr, unsigned long size)
{
	return find_next_bit(addr, size, 0);
}

#define for_each_set_bit(bit, addr, size)                       \
	for ((bit) = find_first_bit((addr), (size));            \
	     (bit) < (size);                                    \
	     (bit) = find_next_bit((addr), (size), (bit) + 1))

/* Софтовый popcount — тела в РЕАЛЬНОМ lib/hweight.c ядра Linux. */
extern unsigned int  __sw_hweight8(unsigned int w);
extern unsigned int  __sw_hweight16(unsigned int w);
extern unsigned int  __sw_hweight32(unsigned int w);
extern unsigned long __sw_hweight64(__u64 w);

#define hweight8(w)     __sw_hweight8(w)
#define hweight16(w)    __sw_hweight16(w)
#define hweight32(w)    __sw_hweight32(w)
#define hweight64(w)    __sw_hweight64(w)
#define hweight_long(w) (BITS_PER_LONG == 32 ? hweight32(w) : hweight64(w))

#endif /* _LINUX_BITOPS_H_SHIM */
