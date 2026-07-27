/* main_pci.c — харнесс PCI поверх driver-model (Веха 67).
 *
 * Синтетическое устройство Intel 8086:100E (82540EM — реальный e1000) вносится в шину PCI, драйвер
 * с id_table регистрируется через pci_register_driver → match по vendor/device → probe. Probe идёт
 * «как e1000»: enable_device → set_master → select/request BAR → ioremap_bar → readl регистра, плюс
 * читает vendor/device из конфиг-пространства и ставит drvdata. Снятие → remove. Обе арх.
 */
#include <stdint.h>
#include <stdio.h>

#include <linux/io.h>
#include <linux/ioport.h>
#include <linux/module.h>
#include <linux/pci.h>

/* «Регистры» устройства в памяти: ioremap identity вернёт указатель прямо на этот буфер. */
#define DEMO_REG_SIGNATURE 0xE1000BA5u
static volatile u32 demo_regs[4] = { DEMO_REG_SIGNATURE, 0, 0, 0 };

/* ─ синтетическое устройство: заполняем как это делал бы PCI-перечислитель ядра ─ */
static struct pci_dev demo_dev = {
	.vendor           = 0x8086,
	.device           = 0x100E,
	.subsystem_vendor = 0x8086,
	.subsystem_device = 0x001E,
	.revision         = 0x03,
	.irq              = 11,
	.lx_name          = "0000:00:03.0",
};

/* ─ драйвер: id_table как у e1000 ({PCI_DEVICE(PCI_VENDOR_ID_INTEL, id)}) ─ */
static const struct pci_device_id demo_id_tbl[] = {
	{ PCI_DEVICE(PCI_VENDOR_ID_INTEL, 0x100E) },
	{ 0, } /* терминатор */
};

static int   probe_called;
static u32   probe_reg;      /* что probe вычитал из BAR */
static u16   probe_vendor, probe_device;
static void *probe_drvdata = (void *)0x1000E;

static int demo_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
	void __iomem *hw;
	int bars;

	probe_called++;
	printf("  probe: %s vendor=%04x device=%04x (id->driver_data=%lu)\n",
	       pci_name(pdev), pdev->vendor, pdev->device, (unsigned long)id->driver_data);

	/* Путь e1000: включить, стать DMA-мастером, взять MEM-BAR'ы, отобразить BAR0. */
	pci_enable_device(pdev);
	pci_set_master(pdev);
	bars = pci_select_bars(pdev, IORESOURCE_MEM);
	pci_request_selected_regions(pdev, bars, "e1000-demo");
	printf("  select_bars(MEM)=0x%x  COMMAND после enable/master: ", bars);
	{ u16 cmd; pci_read_config_word(pdev, PCI_COMMAND, &cmd); printf("0x%04x\n", cmd); }

	hw = pci_ioremap_bar(pdev, 0);
	probe_reg = readl(hw + 0); /* сигнатура «регистра» */
	printf("  ioremap_bar(0) → readl(+0) = 0x%08x\n", probe_reg);

	/* Конфиг-пространство: те же vendor/device, что видел match. */
	pci_read_config_word(pdev, PCI_VENDOR_ID, &probe_vendor);
	pci_read_config_word(pdev, PCI_DEVICE_ID, &probe_device);

	pci_set_drvdata(pdev, probe_drvdata);
	return 0;
}

static int  remove_called;
static void demo_remove(struct pci_dev *pdev)
{
	remove_called++;
	printf("  remove: %s, drvdata=%p\n", pci_name(pdev), pci_get_drvdata(pdev));
}

static struct pci_driver demo_driver = {
	.name     = "e1000-demo",
	.id_table = demo_id_tbl,
	.probe    = demo_probe,
	.remove   = demo_remove,
	.driver.pm = pm_sleep_ptr(0), /* как .driver.pm у e1000 — без CONFIG_PM_SLEEP это NULL */
};

/* «Модуль»: module_init → pci_register_driver (как e1000_init_module). */
static int demo_init(void)
{
	printf("  module_init → pci_register_driver('%s')\n", demo_driver.name);
	return pci_register_driver(&demo_driver);
}
module_init(demo_init);

int main(void)
{
	int ok;

	printf("== Lx_kit PCI поверх driver-model (Веха 67) ==\n");

	/* Разложим конфиг-пространство, как это сделал бы PCI-перечислитель. */
	pci_write_config_word(&demo_dev, PCI_VENDOR_ID, demo_dev.vendor);
	pci_write_config_word(&demo_dev, PCI_DEVICE_ID, demo_dev.device);
	pci_write_config_byte(&demo_dev, PCI_REVISION_ID, demo_dev.revision);

	/* BAR0 → окно «регистров» demo_regs (identity-ioremap отдаст его адрес). */
	demo_dev.resource[0].start = (resource_size_t)(uintptr_t)demo_regs;
	demo_dev.resource[0].end   = demo_dev.resource[0].start + sizeof(demo_regs) - 1;
	demo_dev.resource[0].flags = IORESOURCE_MEM;

	/* Устройство на шине есть, драйвера ещё нет — probe НЕ должен вызваться. */
	lx_pci_register_device(&demo_dev);
	printf("после lx_pci_register_device (драйвера нет): probe_called=%d (ждём 0)\n", probe_called);

	/* module_init регистрирует драйвер → match по id_table → probe. */
	lx_module_init();
	printf("после lx_module_init: probe_called=%d, vendor=%04x device=%04x reg=0x%08x drvdata=%p\n",
	       probe_called, probe_vendor, probe_device, probe_reg, pci_get_drvdata(&demo_dev));

	/* Снятие устройства → remove. */
	pci_unregister_driver(&demo_driver);
	printf("после pci_unregister_driver: remove_called=%d, связь=%s\n",
	       remove_called, demo_dev.dev.driver ? "есть" : "нет");

	ok = probe_called == 1 && remove_called == 1 &&
	     probe_vendor == 0x8086 && probe_device == 0x100E &&
	     probe_reg == DEMO_REG_SIGNATURE && demo_dev.dev.driver == NULL;
	printf("Результат: probe=%d remove=%d vendor=%04x device=%04x reg=0x%08x — %s\n",
	       probe_called, remove_called, probe_vendor, probe_device, probe_reg, ok ? "OK" : "FAIL");
	return 0;
}
