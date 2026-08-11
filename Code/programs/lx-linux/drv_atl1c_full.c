/* drv_atl1c_full.c — запуск НАСТОЯЩЕГО atl1c_probe на живой AR8151 (Веха 133).
 *
 * Отличие от `drv_atl1c.c` (первый контакт): там мы звали отдельные функции atl1c_hw.c, а тут
 * отдаём управление самому драйверу. Иначе нельзя: кольца дескрипторов строит
 * `atl1c_setup_ring_resources`, а она — как и настройка движков TX/RX — объявлена static.
 * Позвать её снаружи невозможно, повторять её у себя значило бы писать свой драйвер вместо
 * хостинга чужого.
 *
 * Как это заводится: собираем `struct pci_dev` руками (перечислителя PCI у нас нет — окно
 * регистров уже выдано ядром под capability), регистрируем его в Lx_kit и зовём module_init.
 * Дальше связка match по id_table → probe работает как в Linux.
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h>

#include "atl1c.h"
#include "lx_sched.h"

#define ATL1C_BAR0_VA 0x50000000UL

extern void lx_net_set_dma_cap(uintptr_t cap);
extern void lx_net_set_irq_cap(uintptr_t cap);
extern int  lx_pci_register_device(struct pci_dev *pdev);
extern int  lx_module_init(void);

/* Устройство, как его заполнил бы перечислитель ядра. Значения — из описи шины (Веха 130):
 * 1969:1083 на 04:00.0, подсистема ASUS, окно регистров 256 КиБ. */
static struct pci_dev g_pdev = {
	.vendor           = PCI_VENDOR_ID_ATTANSIC,
	.device           = PCI_DEVICE_ID_ATHEROS_L1D_2_0,
	.subsystem_vendor = 0x1043,
	.subsystem_device = 0x13c7,
	.revision         = 0x00,
	.irq              = 5,
	.lx_name          = "0000:04:00.0",
};

/* Подъём драйвера: module_init → pci_register_driver → match по id_table → atl1c_probe. */
static void atl1c_bringup(void *arg)
{
	int err;

	(void)arg;
	printk("[atl1c] зову module_init → pci_register_driver → probe\n");
	lx_module_init();
	printk("[atl1c] probe отработал\n");

	/* ПОДНЯТЬ ИНТЕРФЕЙС. probe только опознаёт карту и заводит netdev; кольца дескрипторов,
	 * буферы приёма и запуск движков делает `ndo_open` — в Linux его зовёт `ip link set up`.
	 * У нас поднимать некому: сетевой службы, знающей про это устройство, ещё нет. Зовём сами —
	 * и это ровно тот шаг, ради которого строилась честная DMA-часть (Веха 133). */
	{
		struct net_device *ndev = pci_get_drvdata(&g_pdev);

		if (!ndev) {
			printk("[atl1c] probe не оставил netdev — поднимать нечего\n");
			return;
		}
		printk("[atl1c] поднимаю интерфейс '%s' (ndo_open)\n", ndev->name);
		if (!ndev->netdev_ops || !ndev->netdev_ops->ndo_open) {
			printk("[atl1c] у драйвера нет ndo_open — это не сетевое устройство?\n");
			return;
		}
		err = ndev->netdev_ops->ndo_open(ndev);
		printk("[atl1c] ndo_open вернул %d (%s)\n", err, err ? "ОШИБКА" : "интерфейс поднят");
		if (err)
			return;
		printk("[atl1c] MAC %pM, несущая %s\n", ndev->dev_addr,
		       netif_carrier_ok(ndev) ? "ЕСТЬ" : "нет");
	}
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);
	uintptr_t dma_cap  = vsys_start_cap(1);
	uintptr_t irq_cap  = vsys_start_cap(2);

	/* Небуферизованный вывод С ПЕРВОЙ СТРОКИ: если probe где-то застрянет, увидеть надо всё
	 * сказанное ДО этого места, а не ничего (Веха 133.1). */
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("[atl1c] полный драйвер: запускаю настоящий probe (Веха 133)\n");

	if (mmio_cap == VOID_NO_CAP) {
		printf("[atl1c] нет MMIO-права — запускать должен init\n");
		return 1;
	}
	if (!vsys_mmio_map(mmio_cap, ATL1C_BAR0_VA)) {
		printf("[atl1c] окно регистров не отобразилось\n");
		return 1;
	}
	lx_net_set_dma_cap(dma_cap);
	lx_net_set_irq_cap(irq_cap);
	/* Прерывание нужно НЕ для порядка: приём кадров у atl1c идёт через NAPI, а будит его
	 * обработчик прерывания. Без IRQ-права драйвер поднимется и будет молчать. */

	/* BAR0 драйвер возьмёт через pci_ioremap_bar → ioremap, а тот у нас отдаёт уже
	 * отображённое окно. Длину объявляем настоящую — по ней драйвер считает границы. */
	g_pdev.resource[0].start = ATL1C_BAR0_VA;
	g_pdev.resource[0].end   = ATL1C_BAR0_VA + 0x40000 - 1;
	g_pdev.resource[0].flags = IORESOURCE_MEM;

	lx_pci_register_device(&g_pdev);
	printf("[atl1c] DMA-право %s, IRQ-право %s\n",
	       dma_cap == VOID_NO_CAP ? "НЕТ" : "есть",
	       irq_cap == VOID_NO_CAP ? "НЕТ" : "есть");

	/* probe идёт ЗАДАЧЕЙ планировщика, а не прямо отсюда (Веха 133.1). Вендорный код зовёт
	 * msleep, wait_event и completion — им нужен тот, кто уступит процессор. Вне задачи msleep
	 * сваливается в честную буси-паузу (терпимо), а ожидание события уступать НЕКОМУ, и подъём
	 * повис бы. Ровно так же устроен харнесс e1000 (Вехи 69–73). */
	lx_task_create(atl1c_bringup, NULL, "atl1c");
	lx_sched_run();
	printf("[atl1c] планировщику больше нечего делать — выходим\n");
	return 0;
}
