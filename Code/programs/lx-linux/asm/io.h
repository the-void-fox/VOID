/* asm/io.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Драйверный OS-adaptation слой (e1000_osdep.h) включает <asm/io.h>; аксессоры regs — в linux/io.h.
 * Плюс phys_to_virt/virt_to_phys (у нас identity) и *_rep для потоковой flash-записи. */
#ifndef _ASM_IO_H_SHIM
#define _ASM_IO_H_SHIM

#include <linux/io.h>

static inline void *phys_to_virt(phys_addr_t p) { return (void *)p; }
static inline phys_addr_t virt_to_phys(const volatile void *v) { return (phys_addr_t)v; }

/* Порт-ввод/вывод x86 (e1000 82547 workaround пишет в конфиг-порт). Тела в lx_kit.c. */
void outl(u32 value, unsigned long port);
u32  inl(unsigned long port);
void outb(u8 value, unsigned long port);
u8   inb(unsigned long port);

/* Потоковые ioread/iowrite (flash EEPROM у e1000): count слов подряд. */
static inline void ioread16_rep(const volatile void *addr, void *buf, unsigned long count)
{
	u16 *p = buf;
	while (count--) *p++ = readw(addr);
}
static inline void iowrite16_rep(volatile void *addr, const void *buf, unsigned long count)
{
	const u16 *p = buf;
	while (count--) writew(*p++, addr);
}

#endif /* _ASM_IO_H_SHIM */
