/* linux/jiffies.h — ШИМ lx_emul (Веха 63), НЕ исходник Linux.
 *
 * Счётчик тиков ядра `jiffies` (монотонно растёт, HZ тиков/сек) + идиомы сравнения времени с
 * учётом переполнения (`time_after`/`time_before`) и конвертации мс/мкс↔jiffies. На нём стоят
 * таймеры (<linux/timer.h>), watchdog'и драйверов и таймауты опроса. У нас `jiffies` двигается
 * от МОНОТОННОГО времени VOID в точках планирования/задержки (см. lx_kit.c).
 */
#ifndef _LINUX_JIFFIES_H_SHIM
#define _LINUX_JIFFIES_H_SHIM

#include <linux/types.h>

#define HZ 100 /* тиков в секунду: 1 jiffy = 10 мс (делит 1000 ­— упрощает конвертации) */

/* Глобальный счётчик тиков. volatile: драйверы крутят по нему таймаут-циклы. */
extern unsigned long volatile jiffies;
u64 get_jiffies_64(void);

/* Сравнения времени со знаковой разностью — корректны при переполнении unsigned long. */
#define time_after(a, b)     ((long)((b) - (a)) < 0)
#define time_before(a, b)    time_after(b, a)
#define time_after_eq(a, b)  ((long)((a) - (b)) >= 0)
#define time_before_eq(a, b) time_after_eq(b, a)

/* Конвертации. Округляем ВВЕРХ: сон/таймаут получаются НЕ КОРОЧЕ запрошенного. */
static inline unsigned long msecs_to_jiffies(unsigned int m)
{
	return (m + (1000u / HZ) - 1) / (1000u / HZ);
}

static inline unsigned int jiffies_to_msecs(unsigned long j)
{
	return (unsigned int)(j * (1000u / HZ));
}

static inline unsigned long usecs_to_jiffies(unsigned int u)
{
	return (u + (1000000u / HZ) - 1) / (1000000u / HZ);
}

#endif /* _LINUX_JIFFIES_H_SHIM */
