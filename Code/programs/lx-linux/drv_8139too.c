/* drv_8139too.c — НЕИЗМЕНЁННЫЙ 8139too.c из Linux на VOID (Веха 193).
 *
 * Зачем именно этот драйвер. Веха 192 свела описание драйвера в одну строку; проверить конвейер
 * можно только добавив ВТОРОЙ класс железа и посмотрев, сколько это стоит. Кандидатов было два:
 *
 *   r8169 — то, что стоит в современных машинах. Но он ходит в PHY через `<linux/phy.h>`, а это
 *           не заголовок, а вход в ПОДСИСТЕМУ phylib: MDIO-шина, автомат состояний устройства,
 *           опрос линка рабочей очередью. Тысячи строк чужого кода сверх драйвера.
 *   8139too — RTL8139, тот же производитель, но старое поколение: PHY у него свой, через
 *           `<linux/mii.h>`, который шим уже даёт. Не хватало трёх мелких заголовков.
 *
 * Взят второй, и не из лени: он измеряет РОВНО то, ради чего затевался конвейер, — цену
 * добавления драйвера того же класса. Вдобавок RTL8139 эмулирует QEMU, а значит проверка не
 * требует живого ноутбука, в отличие от atl1c. r8169 остаётся отдельной вехой, и её настоящее
 * содержание — phylib, а не драйвер.
 *
 * Как это заводится — как у atl1c (Веха 133): собираем `struct pci_dev` руками (перечислителя
 * PCI у нас нет, окно регистров уже выдано ядром под capability), регистрируем в Lx_kit, зовём
 * module_init. Дальше match по id_table → probe работает как в Linux.
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h>

#include "lx_sched.h"

#include <linux/delay.h>
#include <linux/etherdevice.h>
#include <linux/netdevice.h>
#include <linux/pci.h>

/* Куда отображаем окно регистров. Адрес тот же по смыслу, что у atl1c, и по той же причине:
 * `ioremap` у нас отдаёт уже отображённое окно, а не отображает сам. */
#define RTL8139_BAR1_VA 0x50000000UL
/* Регистровый файл RTL8139 — 256 байт. Драйвер сверяет длину с RTL_MIN_IO_SIZE и откажется,
 * если объявить меньше. */
#define RTL8139_BAR1_LEN 0x100

extern void lx_net_set_dma_cap(uintptr_t cap);
extern void lx_net_set_irq_cap(uintptr_t cap);
extern void lx_net_set_netdev_cap(uintptr_t cap);
extern int  lx_netdev_attach(struct net_device *dev);
extern int  lx_netdev_open(struct net_device *dev);
extern int  lx_pci_register_device(struct pci_dev *pdev);
extern void lx_pci_set_cfg_cap(uintptr_t cap);
extern int  lx_module_init(void);

/* Устройство, как его заполнил бы перечислитель ядра. 10ec:8139 — RTL8139, то же, что отдаёт
 * QEMU по `-device rtl8139`. */
static struct pci_dev g_pdev = {
	.vendor  = 0x10ec,
	.device  = 0x8139,
	.subsystem_vendor = 0x10ec,
	.subsystem_device = 0x8139,
	.revision = 0x10,
	.irq      = 11,
	.lx_name  = "0000:00:03.0",
};

/* Сторож: держит процесс живым. Ровно та же причина, что у atl1c, и она не про удобство —
 * карта DMA'ит в НАШУ память. Умри процесс, ядро вернёт эти страницы в общий котёл, а карта
 * продолжит писать в них принятые кадры: порча чужой памяти вдалеке от причины.
 *
 * Веха 195 — спит теперь СЕКУНДУ, а не минуту. Дело не в сторожении: холостой путь планировщика
 * выбирает между «спать до срока таймера» и «спать до прерывания», и пока здесь стояла минута,
 * он выбирал первое — карта могла принять кадр, а узнали бы мы об этом через минуту. Секунда
 * стоит одного пробуждения в секунду и оставляет прерывание рабочим путём.
 */
static void rtl8139_keepalive(void *arg)
{
	(void)arg;
	for (;;)
		msleep(1000);
}

static void rtl8139_bringup(void *arg)
{
	struct net_device *ndev;

	(void)arg;
	printk("[8139too] зову module_init → pci_register_driver → probe\n");
	lx_module_init();
	printk("[8139too] probe отработал\n");

	ndev = pci_get_drvdata(&g_pdev);
	if (!ndev) {
		printk("[8139too] probe не оставил netdev — поднимать нечего\n");
		return;
	}
	/* `probe` только опознаёт карту и заводит netdev; кольца, буферы приёма и запуск движков
	 * делает `ndo_open` — в Linux его зовёт `ip link set up`. У нас звать некому: сетевой
	 * службы, знающей про это устройство, нет.
	 *
	 * Веха 195 — зовём не `ndo_open`, а `lx_netdev_open`: кроме открытия он ставит `IFF_UP`,
	 * как это делает `dev_open` в Linux. Без флага обработчик прерывания драйвера отказывался
	 * работать («интерфейс выключен»), и приёма не было вовсе. */
	{
		int err = lx_netdev_open(ndev);

		printk("[8139too] интерфейс поднят → %d\n", err);
		/* Объявиться КАРТОЙ СИСТЕМЫ, и только после открытия: до него кольца не построены,
		 * приёмник стоит, а стек уже начал бы слать в пустоту. С этого мгновения принятые
		 * кадры уходят в ядро, а исходящие приходят оттуда. */
		if (!err)
			lx_netdev_attach(ndev);
	}
	lx_task_create(rtl8139_keepalive, NULL, "8139-idle");
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);
	uintptr_t dma_cap  = vsys_start_cap(1);
	uintptr_t irq_cap  = vsys_start_cap(2);
	uintptr_t ndev_cap = vsys_start_cap(3); /* Веха 195 — право БЫТЬ картой системы */

	/* Небуферизованный вывод С ПЕРВОЙ СТРОКИ: застрянь probe — увидеть надо всё сказанное до
	 * этого места, а не ничего (урок Вехи 133.1). */
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("[8139too] неизменённый драйвер Linux: запускаю probe (Веха 193)\n");

	if (mmio_cap == VOID_NO_CAP) {
		printf("[8139too] нет MMIO-права — запускать должен init\n");
		return 1;
	}
	if (!vsys_mmio_map(mmio_cap, RTL8139_BAR1_VA)) {
		printf("[8139too] окно регистров не отобразилось\n");
		return 1;
	}
	lx_net_set_dma_cap(dma_cap);
	lx_net_set_irq_cap(irq_cap);
	lx_net_set_netdev_cap(ndev_cap);

	/* Драйвер берёт BAR1 (MMIO): у него `bar = !use_io`, а `use_io` по умолчанию ложь.
	 * BAR0 — порты ввода-вывода, и трогать их нам нечем. */
	g_pdev.resource[1].start = RTL8139_BAR1_VA;
	g_pdev.resource[1].end   = RTL8139_BAR1_VA + RTL8139_BAR1_LEN - 1;
	g_pdev.resource[1].flags = IORESOURCE_MEM;

	/* Веха 199.11 — конфиг PCI настоящий (по праву на окно регистров). До этого `pci_set_master`
	 * писал в массив в памяти процесса, то есть bus master включала только прошивка. */
	lx_pci_set_cfg_cap(mmio_cap);

	lx_pci_register_device(&g_pdev);
	printf("[8139too] DMA-право %s, IRQ-право %s, право-карта %s\n",
	       dma_cap == VOID_NO_CAP ? "НЕТ" : "есть",
	       irq_cap == VOID_NO_CAP ? "НЕТ" : "есть",
	       ndev_cap == VOID_NO_CAP ? "НЕТ" : "есть");

	/* probe идёт ЗАДАЧЕЙ планировщика, а не прямо отсюда: вендорный код зовёт msleep и
	 * ожидания, а им нужен тот, кто уступит процессор. */
	lx_task_create(rtl8139_bringup, NULL, "8139too");
	lx_sched_run();
	printf("[8139too] планировщику больше нечего делать — выходим\n");
	return 0;
}
