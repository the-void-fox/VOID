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

#include <linux/etherdevice.h>
#include <linux/skbuff.h>

#define ATL1C_BAR0_VA 0x50000000UL

/* ─── ЖУРНАЛ ПО ПРОВОДУ (Веха 134) ───────────────────────────────────────────
 *
 * Зачем. Каждая проверка на этой машине стоит перезагрузки и фотографии экрана: COM-порта нет,
 * USB для VOID не блочное устройство, а видеть надо весь журнал ядра. Как только карта начала
 * передавать, всё это решается само: шлём журнал СЫРЫМИ Ethernet-кадрами прямо в провод.
 *
 * Ни IP, ни ARP, ни DHCP — их у нас ещё нет, и здесь они не нужны: широковещательный кадр со
 * своим EtherType доходит до соседа по кабелю без всякой настройки. На той стороне слушает
 * `Code/tools/netlog.py`.
 *
 * EtherType 0x88B5 — из диапазона, отведённого IEEE под опытное и местное применение. Занимать
 * ради отладки чужой номер нельзя, а этот для того и заведён.
 */
#define VOID_LOG_ETHERTYPE 0x88b5
#define VOID_LOG_CHUNK     1024 /* полезная нагрузка кадра — с запасом под MTU 1500 */

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

/* Отправить один кадр с куском журнала. 0 — карта его приняла. */
static int netlog_send(struct net_device *ndev, const unsigned char *data, unsigned len)
{
	struct sk_buff *skb;
	unsigned char *p;

	/* Буфер берём через netdev_alloc_skb: он выделяет ИЗ АРЕНЫ DMA, а значит адрес этой памяти
	 * можно назвать карте (см. lx_net.c). Память из обычной кучи здесь не годится вовсе. */
	skb = __netdev_alloc_skb(ndev, ETH_HLEN + len, 0);
	if (!skb)
		return -1;

	p = skb_put(skb, ETH_HLEN + len);
	memset(p, 0xff, ETH_ALEN);                       /* всем: соседа по кабелю мы не знаем */
	memcpy(p + ETH_ALEN, ndev->dev_addr, ETH_ALEN);
	p[12] = (unsigned char)(VOID_LOG_ETHERTYPE >> 8);
	p[13] = (unsigned char)(VOID_LOG_ETHERTYPE & 0xff);
	memcpy(p + ETH_HLEN, data, len);

	skb->dev = ndev;
	return ndev->netdev_ops->ndo_start_xmit(skb, ndev) == NETDEV_TX_OK ? 0 : -1;
}

/* Задача-вещатель: следит за журналом ядра и отправляет всё, что в нём появилось. */
static void netlog_task(void *arg)
{
	struct net_device *ndev = arg;
	static unsigned char log[64 * 1024];
	size_t sent = 0;
	unsigned rounds = 0, frames = 0, failed = 0;

	for (;;) {
		size_t n = vsys_klog(log, sizeof(log));
		size_t off;

		/* Раз в пять секунд — отчёт О СЕБЕ на экран машины. Пока журнал по проводу не поехал,
		 * узнать, что с ним, можно только отсюда: сам он себя доставить не может. Счётчик
		 * отданных карте кадров рядом со счётчиком отказов отвечает на главный вопрос — молчит
		 * карта или молчит провод. */
		if (++rounds % 25 == 0) {
			struct atl1c_adapter *ad = netdev_priv(ndev);
			struct atl1c_tpd_ring *tx = &ad->tpd_ring[0];
			u32 mac_ctrl = 0;
			u16 hw_cons = 0;

			/* Спрашиваем САМУ КАРТУ, а не свои счётчики. Наши говорят лишь то, что кадр
			 * отдан драйверу; вопрос же в том, забрала ли его карта.
			 *
			 *   - `next_to_use` — куда МЫ положили последний дескриптор;
			 *   - `tpd_cons`    — до какого места дошла КАРТА (её собственный указатель);
			 *   - MAC_CTRL      — включён ли вообще передатчик.
			 *
			 * Если наш указатель ушёл вперёд, а карта стоит на нуле — она не читает кольцо,
			 * и разговор про DMA. Если оба идут, а на проводе тихо — разговор про провод. */
			/* Таблица очередей `atl1c_qregs` объявлена static — берём регистр очереди 0
			 * напрямую по имени, оно из того же заголовка. */
			AT_READ_REG(&ad->hw, REG_MAC_CTRL, &mac_ctrl);
			AT_READ_REGW(&ad->hw, REG_TPD_PRI0_CIDX, &hw_cons);
			printk("[netlog] отдано %u, отказов %u; кольцо: мы %u, карта %u; MAC_CTRL %08x (TX %s, RX %s)\n",
			       frames, failed, (unsigned)tx->next_to_use, (unsigned)hw_cons,
			       (unsigned)mac_ctrl,
			       (mac_ctrl & MAC_CTRL_TX_EN) ? "вкл" : "ВЫКЛ",
			       (mac_ctrl & MAC_CTRL_RX_EN) ? "вкл" : "ВЫКЛ");
		}

		/* Журнал укоротился — значит кольцо провернулось и часть мы потеряли. Начинаем с
		 * начала снимка: слать по второму разу всё незачем, а притворяться, что потери не
		 * было, нельзя — она видна по разрыву в тексте. */
		if (n < sent)
			sent = 0;

		for (off = sent; off < n;) {
			unsigned chunk = (unsigned)(n - off);

			if (chunk > VOID_LOG_CHUNK)
				chunk = VOID_LOG_CHUNK;
			if (netlog_send(ndev, log + off, chunk) != 0) {
				failed++;
				break; /* карта не приняла — повторим в следующий заход */
			}
			frames++;
			off += chunk;
		}
		sent = off;
		msleep(200);
	}
}

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
		printk("[atl1c] --- вход в ndo_open ---\n");
		err = ndev->netdev_ops->ndo_open(ndev);
		printk("[atl1c] --- выход из ndo_open ---\n");
		printk("[atl1c] ndo_open вернул %d (%s)\n", err, err ? "ОШИБКА" : "интерфейс поднят");
		if (err)
			return;
		printk("[atl1c] MAC %pM, несущая %s\n", ndev->dev_addr,
		       netif_carrier_ok(ndev) ? "ЕСТЬ" : "нет");

		/* РАЗГОВОРЧИВОСТЬ НА ПОЛНУЮ. У драйвера есть путь, где кадр выбрасывается, а ответ
		 * всё равно «принято»:
		 *
		 *     if (atl1c_tx_map(...) < 0) { netif_info(adapter, tx_done, …); … }
		 *     return NETDEV_TX_OK;
		 *
		 * и это сообщение гасится, потому что класс `tx_done` в msg_enable по умолчанию не
		 * включён. То есть передача может молча не состояться, а мы будем считать её удачной.
		 * Включаем все классы: на bring-up'е лишняя строка стоит ничего, а пропущенная —
		 * перезагрузки. */
		{
			struct atl1c_adapter *ad = netdev_priv(ndev);

			ad->msg_enable = 0xffff;
		}

		/* Первый кадр — приметный: по нему на той стороне видно, что провод живой, ещё до
		 * того, как поедет журнал. Длину берём у самой строки: считать её руками — верный
		 * способ отправить в провод четыре лишних байта чужой памяти (уже отправлял). */
		{
			static const char hello[] = "VOID: провод живой, начинаю вещать журнал\n";

			if (netlog_send(ndev, (const unsigned char *)hello,
					(unsigned)(sizeof(hello) - 1)) == 0)
				printk("[atl1c] пробный кадр отдан карте\n");
			else
				printk("[atl1c] пробный кадр карта НЕ приняла\n");
		}

		lx_task_create(netlog_task, ndev, "netlog");
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
