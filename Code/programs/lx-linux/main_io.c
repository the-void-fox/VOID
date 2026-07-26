/* main_io.c — харнесс (Веха 60): прогоняем linux/io.h — MMIO-аксессоры регистров.
 *
 * io.h — инфраструктурный заголовок: readl/writel и родня. Здесь харнесс трактует локальный буфер
 * как блок регистров устройства и проверяет round-trip чтения/записи всех ширин + байтовую раскладку
 * (little-endian на наших архах). Настоящее окно регистров даст lx_emul по MMIO-cap; аксессоры — те же.
 */
#include <linux/io.h>     /* readl/writel/… */
#include <linux/printk.h> /* printk() — из Lx_kit */

int main(void)
{
	/* Буфер-«регистры»: выровнен под u64, трактуем как MMIO-окно. */
	static volatile unsigned long long block[4];
	volatile void *regs = (volatile void *)block;
	int ok = 1;

	block[0] = block[1] = block[2] = block[3] = 0;

	/* 32-битная запись/чтение + байтовая раскладка LE. */
	writel(0xDEADBEEFu, regs);
	if (readl(regs) != 0xDEADBEEFu) ok = 0;
	if (readb(regs) != 0xEFu || readb((const volatile char *)regs + 1) != 0xBEu ||
	    readb((const volatile char *)regs + 3) != 0xDEu)
		ok = 0; /* младший байт по младшему адресу */

	/* 16- и 8-битные аксессоры по смещениям. */
	writew(0x1234u, (volatile char *)regs + 8);
	if (readw((const volatile char *)regs + 8) != 0x1234u) ok = 0;
	if (readb((const volatile char *)regs + 8) != 0x34u) ok = 0;

	writeb(0xA5u, (volatile char *)regs + 10);
	if (readb((const volatile char *)regs + 10) != 0xA5u) ok = 0;

	/* 64-битный round-trip + ioread/iowrite и _relaxed. */
	writeq(0x0123456789ABCDEFull, (volatile char *)regs + 16);
	if (readq((const volatile char *)regs + 16) != 0x0123456789ABCDEFull) ok = 0;
	iowrite32(0xCAFEBABEu, (volatile char *)regs + 24);
	if (ioread32((const volatile char *)regs + 24) != 0xCAFEBABEu) ok = 0;
	if (readl_relaxed((const volatile char *)regs + 24) != 0xCAFEBABEu) ok = 0;

	printk("[lx-io] linux/io.h на VOID: writel(0xDEADBEEF)→readl=0x%08X, LE-байт[0]=0x%02X, "
	       "readq=0x%016llX\n",
	       readl(regs), readb(regs),
	       (unsigned long long)readq((const volatile char *)regs + 16));
	printk("[lx-io] MMIO-аксессоры (readl/writel/…) %s -- io.h ядра Linux РАБОТАЮТ на VOID\n",
	       ok ? "верны" : "НЕВЕРНЫ");

	return ok ? 0 : 1;
}
