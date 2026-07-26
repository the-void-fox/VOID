/* main_argv.c — харнесс (Веха 56): наша программа VOID зовёт НЕИЗМЕНЁННЫЙ lib/argv_split.c ядра.
 *
 * argv_split.c (linux-src/, verbatim Linux 6.18.7) режет строку по пробелам в argv-массив, честно
 * АЛЛОЦИРУЯ память ядровым kmalloc/kstrndup — который у нас теперь даёт Lx_kit (lx_kit.c) поверх
 * кучи VOID. Так проверяется фундамент kit: реальный код ядра, опирающийся на аллокатор и строки,
 * работает. Печать — ядровым printk (тоже Lx_kit). Шаг вверх от Вехи 55: sort.c ничего не аллоцировал.
 */
#include <linux/slab.h>   /* GFP_KERNEL */
#include <linux/string.h> /* argv_split()/argv_free() — из настоящего ядра Linux; strcmp — newlib */
#include <linux/printk.h> /* printk() — из Lx_kit */

int main(void)
{
	const char *line = "  eth0   up   mtu 1500  "; /* нарочно кривые/двойные пробелы по краям */
	const char *want[] = { "eth0", "up", "mtu", "1500" };
	int argc = -1, i, ok = 1;

	char **argv = argv_split(GFP_KERNEL, line, &argc); /* ← вызов реального кода ядра Linux */
	if (!argv) {
		printk("[lx-argv] argv_split вернул NULL -- аллокатор не дал памяти?\n");
		return 1;
	}

	if (argc != 4)
		ok = 0;
	for (i = 0; i < argc; i++)
		if (!argv[i] || strcmp(argv[i], want[i]) != 0)
			ok = 0;
	if (argv[argc] != NULL) /* argv обязан быть NULL-терминирован */
		ok = 0;

	printk("[lx-argv] Linux lib/argv_split.c (6.18.7, неизменённый) на VOID: argc=%d [", argc);
	for (i = 0; i < argc; i++)
		printk("'%s'%s", argv[i], i + 1 < argc ? ", " : "");
	printk("]\n[lx-argv] разбор %s -- аллокатор ядра (kmalloc) + строки РАБОТАЮТ на VOID\n",
	       ok ? "верен" : "НЕВЕРЕН");

	argv_free(argv); /* ← ядровый kfree через Lx_kit */
	return ok ? 0 : 1;
}
