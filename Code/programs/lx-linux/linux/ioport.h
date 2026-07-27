/* linux/ioport.h — ШИМ lx_emul (Веха 67), НЕ исходник Linux.
 *
 * `struct resource` — диапазон адресов устройства (окно регистров MMIO, порт ввода-вывода). У PCI
 * это BAR'ы (`pci_dev->resource[]`). Флаги IORESOURCE_* различают тип. У нас — только описание
 * диапазона (реальное окно даёт ioremap по MMIO-cap VOID); менеджера ресурсов не держим.
 */
#ifndef _LINUX_IOPORT_H_SHIM
#define _LINUX_IOPORT_H_SHIM

#include <linux/types.h>
#include <linux/io.h> /* resource_size_t */

struct resource {
	resource_size_t start;
	resource_size_t end;
	const char     *name;
	unsigned long   flags;
};

#define IORESOURCE_IO      0x00000100
#define IORESOURCE_MEM     0x00000200
#define IORESOURCE_REG     0x00000300
#define IORESOURCE_IRQ     0x00000400
#define IORESOURCE_DMA     0x00000800
#define IORESOURCE_PREFETCH 0x00002000

static inline resource_size_t resource_size(const struct resource *res)
{
	return res->end - res->start + 1;
}

#endif /* _LINUX_IOPORT_H_SHIM */
