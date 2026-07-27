/* linux/delay.h — ШИМ lx_emul (Веха 61), НЕ исходник Linux.
 *
 * Задержки для тайминга железа: udelay/ndelay/mdelay — БУСИ-ожидание (в ядре зовутся и в атомарном
 * контексте, спать нельзя), msleep — «сон». Тела — в Lx_kit (lx_kit.c): буси-ожидание по МОНОТОННОМУ
 * времени VOID (gettimeofday → vsys_ticks, разрешение 1–100 нс). Так драйвер получает честные паузы
 * для последовательностей инициализации/поллинга регистров. msleep пока тоже буси (планировщик
 * Lx_kit — позже; тогда станет уступающим).
 */
#ifndef _LINUX_DELAY_H_SHIM
#define _LINUX_DELAY_H_SHIM

void ndelay(unsigned long nsecs);
void udelay(unsigned long usecs);
void mdelay(unsigned long msecs);
void msleep(unsigned int msecs);

/* usleep_range — «сон» в диапазоне мкс; у нас буси-udelay по нижней границе. */
static inline void usleep_range(unsigned long min, unsigned long max) { (void)max; udelay(min); }

#endif /* _LINUX_DELAY_H_SHIM */
