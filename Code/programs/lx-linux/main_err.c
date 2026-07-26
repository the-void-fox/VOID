/* main_err.c — харнесс (Веха 59): прогоняем linux/err.h — идиому ядра «ошибка в указателе».
 *
 * err.h — инфраструктурный заголовок (не привязан к отдельному .c ядра): его проверяет харнесс,
 * а в бою он раскроется, когда портируемый драйвер станет возвращать ERR_PTR(-Exxx). Здесь —
 * прямая проверка семантики: кодирование/распознавание кода ошибки в указателе, границы диапазона.
 */
#include <linux/err.h>    /* ERR_PTR/PTR_ERR/IS_ERR/… + коды errno через <linux/errno.h> */
#include <linux/printk.h> /* printk() — из Lx_kit */

int main(void)
{
	int real = 42;
	void *valid = &real;
	void *enomem = ERR_PTR(-ENOMEM);
	void *einval = ERR_PTR(-EINVAL);
	int ok = 1;

	/* Код ошибки: распознаётся как ошибка и извлекается обратно. */
	if (!IS_ERR(enomem) || PTR_ERR(enomem) != -ENOMEM) ok = 0;
	if (!IS_ERR(einval) || PTR_ERR(einval) != -EINVAL) ok = 0;

	/* Настоящий указатель: НЕ ошибка и НЕ NULL. */
	if (IS_ERR(valid) || IS_ERR_OR_NULL(valid)) ok = 0;

	/* NULL: не «ошибка», но ловится IS_ERR_OR_NULL. */
	if (IS_ERR(NULL) || !IS_ERR_OR_NULL(NULL)) ok = 0;

	/* PTR_ERR_OR_ZERO: 0 на валидном, код — на ошибке. */
	if (PTR_ERR_OR_ZERO(valid) != 0) ok = 0;
	if (PTR_ERR_OR_ZERO(einval) != -EINVAL) ok = 0;

	printk("[lx-err] linux/err.h на VOID: ERR_PTR(-ENOMEM) → IS_ERR=%d PTR_ERR=%ld; "
	       "valid → IS_ERR=%d; NULL → IS_ERR_OR_NULL=%d\n",
	       IS_ERR(enomem), PTR_ERR(enomem), IS_ERR(valid), IS_ERR_OR_NULL(NULL));
	printk("[lx-err] идиома «ошибка в указателе» %s -- err.h ядра Linux РАБОТАЕТ на VOID\n",
	       ok ? "верна" : "НЕВЕРНА");

	return ok ? 0 : 1;
}
