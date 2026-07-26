/* main_bits.c — харнесс (Веха 58): наша программа VOID прогоняет linux/bitops.h + НЕИЗМЕНЁННЫЙ
 * lib/hweight.c ядра.
 *
 * Битовые операции — вездесущи в драйверах. Здесь харнесс гоняет наш шим bitops.h (BIT/GENMASK,
 * set/clear/test_bit, for_each_set_bit, ffs/fls), а вызовы hweight маршрутизируются в РЕАЛЬНЫЙ
 * lib/hweight.c (linux-src/, verbatim Linux 6.18.7) — софтовый popcount ядра. Так одновременно
 * доказан шим-заголовок И работает настоящий код ядра. Следующий кирпич kit после списка (Веха 57).
 */
#include <linux/bitops.h> /* наш шим (+ extern __sw_hweight* из hweight.c) */
#include <linux/printk.h> /* printk() — из Lx_kit */

int main(void)
{
	DECLARE_BITMAP(bm, 128) = { 0 };
	unsigned int want[] = { 3, 7, 64, 100 }; /* какие биты выставим */
	unsigned int got[8];
	unsigned long bit;
	int n = 0, ok = 1, i;

	set_bit(3, bm);
	set_bit(7, bm);
	set_bit(64, bm);
	set_bit(100, bm);

	/* test_bit: выставленные — есть, соседние — нет. */
	if (!test_bit(3, bm) || !test_bit(64, bm) || test_bit(5, bm) || test_bit(99, bm))
		ok = 0;

	/* test_and_clear_bit возвращает старое значение и гасит бит. */
	if (!test_and_clear_bit(7, bm) || test_bit(7, bm))
		ok = 0;
	set_bit(7, bm); /* вернём обратно для обхода ниже */

	/* for_each_set_bit обходит выставленные по возрастанию. */
	for_each_set_bit(bit, bm, 128) {
		if (n < 8)
			got[n] = (unsigned int)bit;
		n++;
	}
	if (n != 4)
		ok = 0;
	else
		for (i = 0; i < 4; i++)
			if (got[i] != want[i])
				ok = 0;

	/* hweight → РЕАЛЬНЫЙ lib/hweight.c ядра. */
	if (hweight32(0xF0F0F0F0u) != 16)  ok = 0; /* 8 групп по 4 бита */
	if (hweight64(~0ULL) != 64)        ok = 0; /* все биты */
	if (hweight16(0xACE1u) != 8)       ok = 0;
	if (hweight8(0xFFu) != 8)          ok = 0;
	if (hweight_long(0xFFUL) != 8)     ok = 0;

	/* Макросы масок/разрядов. */
	if (BIT(5) != 32UL || GENMASK(7, 4) != 0xF0UL) ok = 0;
	if (fls(0x80u) != 8 || __ffs(0x80UL) != 7)     ok = 0;

	printk("[lx-bits] linux/bitops.h + Linux lib/hweight.c (6.18.7, неизменённый) на VOID: биты [");
	for (i = 0; i < n && i < 8; i++)
		printk("%u%s", got[i], i + 1 < n ? " " : "");
	printk("], hweight64(~0)=%lu, hweight32(0xF0F0F0F0)=%u\n", hweight64(~0ULL), hweight32(0xF0F0F0F0u));
	printk("[lx-bits] битовые операции %s -- bitops ядра Linux РАБОТАЮТ на VOID\n",
	       ok ? "верны" : "НЕВЕРНЫ");

	return ok ? 0 : 1;
}
