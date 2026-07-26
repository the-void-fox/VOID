/* main_delay.c — харнесс (Веха 61): прогоняем linux/delay.h — честные паузы тайминга железа.
 *
 * delay.h — инфраструктурный заголовок: udelay/mdelay/ndelay. Здесь харнесс замеряет монотонное
 * время VOID (gettimeofday) до и после реальной паузы и проверяет, что прошло НЕ МЕНЬШЕ запрошенного
 * (буси-ожидание Lx_kit крутится по тому же источнику времени). Так драйвер получает рабочие паузы
 * для последовательностей инициализации/поллинга.
 */
#include <sys/time.h>

#include <linux/delay.h>  /* udelay/mdelay/ndelay — тела в Lx_kit */
#include <linux/printk.h> /* printk() — из Lx_kit */

static unsigned long long now_us(void)
{
	struct timeval tv;
	gettimeofday(&tv, NULL);
	return (unsigned long long)tv.tv_sec * 1000000ull + (unsigned long long)tv.tv_usec;
}

int main(void)
{
	unsigned long long t0, dt_m, dt_u;
	int ok = 1;

	t0 = now_us();
	mdelay(50); /* 50 мс */
	dt_m = now_us() - t0;

	t0 = now_us();
	udelay(3000); /* 3 мс */
	dt_u = now_us() - t0;

	/* Прошло не меньше запрошенного (с запасом на разрешение/накладные — верхнюю границу не
	 * проверяем: QEMU/TCG может тормозить, важно что задержка РЕАЛЬНО состоялась). */
	if (dt_m < 50000ull)  ok = 0;
	if (dt_u < 3000ull)   ok = 0;

	printk("[lx-delay] linux/delay.h на VOID: mdelay(50 мс) занял %llu мкс, udelay(3000 мкс) — %llu мкс\n",
	       dt_m, dt_u);
	printk("[lx-delay] задержки (буси-ожидание по монотонному времени) %s -- delay.h ядра Linux РАБОТАЕТ на VOID\n",
	       ok ? "верны" : "НЕВЕРНЫ");

	return ok ? 0 : 1;
}
