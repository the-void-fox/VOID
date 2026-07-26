/* linux/gfp.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * Флаги-подсказки аллокатору (в ядре — приоритет/контекст/зона памяти при выделении). У нас
 * единая куча newlib (_sbrk поверх одного SYS_MAP), контекст исполнения один — флаги ни на что
 * не влияют, аллокатор Lx_kit их игнорирует. Тип gfp_t и имена нужны лишь чтобы неизменённый код
 * ядра компилировался и читался как в Linux. Растёт по мере надобности.
 */
#ifndef _LINUX_GFP_H_SHIM
#define _LINUX_GFP_H_SHIM

typedef unsigned int gfp_t;

#define __GFP_ZERO ((gfp_t)0x100u)
#define GFP_ATOMIC ((gfp_t)0x020u)
#define GFP_KERNEL ((gfp_t)0x000u)
#define GFP_NOWAIT ((gfp_t)0x000u)
#define GFP_DMA    ((gfp_t)0x001u)

#endif /* _LINUX_GFP_H_SHIM */
