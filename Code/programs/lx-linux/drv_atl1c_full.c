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

#include <linux/delay.h>
#include <linux/etherdevice.h>
#include <linux/mii.h>
#include <linux/skbuff.h>

#define ATL1C_BAR0_VA 0x50000000UL

/* Вещание журнала в провод здесь БЫЛО и УБРАНО (Веха 134.5, решение владельца).
 *
 * Замысел был такой: раз карта начала передавать, пусть шлёт журнал ядра сырыми кадрами — и
 * отладка на этой машине перестанет упираться в фотографии экрана. Работать это не начало
 * (карта забирала дескрипторы, а на проводе было пусто), но убрано не поэтому.
 *
 * Убрано потому, что затея НЕБЕЗОПАСНА по устройству: журнал уходил ШИРОКОВЕЩАТЕЛЬНО, то есть
 * любому на сегменте. Пока это провод до соседней машины — терпимо; воткни тот же кабель в
 * роутер — и журнал ядра читает вся сеть. Вдобавок вещание включалось само, без выключателя в
 * конфиге. В системе, которая строится вокруг «ничего не происходит без явного права», такому
 * места нет.
 *
 * Если удалённый журнал понадобится снова — делать его надо иначе: адресно (а не всем), под
 * явным правом из конфига поколения и с выключателем по умолчанию.
 */

extern void lx_net_set_dma_cap(uintptr_t cap);
extern void lx_net_set_irq_cap(uintptr_t cap);
extern void lx_net_set_netdev_cap(uintptr_t cap);
extern void lx_net_set_diag(void (*fn)(void));
extern int  lx_netdev_open(struct net_device *dev);
extern int  lx_netdev_attach(struct net_device *dev);
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

/* ─── Веха 199.7 — РАССКАЗ САМОЙ КАРТЫ ───────────────────────────────────────────────────────
 *
 * Пульс шима (`lx_net_pulse`) считает НАШ путь: звали ли обработчик, планировался ли опрос,
 * дошёл ли кадр до стека. Три захода подряд он показывал одно и то же — путь исправен весь,
 * кадров нет, — и этого оказалось мало: карта может честно принимать с провода и столь же
 * честно выбрасывать принятое по СВОИМ правилам, ничего никому не сказав.
 *
 * Спросить об этом можно только её саму. У MAC есть собственные счётчики приёма (0x1700…),
 * и они отвечают на единственный вопрос, который нельзя вывести ниоткуда больше: доходит ли
 * кадр до карты вообще. Дальше картина читается однозначно:
 *
 *   принято 0 и все ошибки 0   — с провода не приходит НИЧЕГО (фильтр, RX выключен, PHY);
 *   «больше предела» растёт    — приходит, но карта считает кадры слишком длинными (REG_MTU);
 *   FCS/выравнивание растут    — приходит мусор (провод, скорость, дуплекс);
 *   принято > 0, а у нас 0     — карта приняла, но не смогла отдать: кольцо или DMA.
 *
 * Счётчики MAC сбрасываются ЧТЕНИЕМ (поэтому в Linux `atl1c_update_hw_stats` их складывает) —
 * складываем и мы. Отсюда же следует, что смотреть на них может только ОДИН читатель; в VOID
 * `ndo_get_stats` не зовёт никто, так что мы здесь одни.
 *
 * Веха 199.8 — и то же самое про ПЕРЕДАЧУ (0x1760…). Приём на машине владельца отработал ровно
 * первую секунду и встал, вместе с передачей; отличить «кадр не ушёл на провод» от «ушёл, но
 * ответа нет» иначе нечем, а это два совершенно разных поиска.
 */
#define AT_R32(off) (*(volatile u32 *)(ATL1C_BAR0_VA + (off)))

/* Счётчики MAC идут подряд по 4 байта в порядке полей `struct atl1c_hw_stats`. Читаем диапазон
 * ЦЕЛИКОМ, а не выборочно: чтение их обнуляет, и пропущенный регистр молча копился бы до
 * переполнения — то есть однажды соврал бы. Имена нужных индексов — ниже. */
#define AT_RX_WORDS ((REG_MAC_RX_STATUS_END - REG_MAC_RX_STATUS_BIN) / 4 + 1)
#define AT_TX_WORDS ((REG_MAC_TX_STATUS_END - REG_MAC_TX_STATUS_BIN) / 4 + 1)

enum { RX_OK = 0, RX_BCAST = 1, RX_FCS = 5, RX_LEN = 6,
       RX_SZ_OV = 17, RX_FIFO_OV = 18, RX_RRD_OV = 19, RX_ALIGN = 20, RX_FILTERED = 23 };
enum { TX_OK = 0, TX_LATE_COL = 17, TX_ABORT_COL = 18, TX_UNDERRUN = 19,
       TX_LEN_ERR = 21, TX_TRUNC = 22 };

static unsigned long at_rx[AT_RX_WORDS], at_tx[AT_TX_WORDS];

static void atl1c_diag(void)
{
	u32 mac = AT_R32(REG_MAC_CTRL);
	u32 rxq = AT_R32(REG_RXQ_CTRL);
	u32 txq = AT_R32(REG_TXQ_CTRL);
	unsigned i;

	for (i = 0; i < AT_RX_WORDS; i++)
		at_rx[i] += AT_R32(REG_MAC_RX_STATUS_BIN + i * 4);
	for (i = 0; i < AT_TX_WORDS; i++)
		at_tx[i] += AT_R32(REG_MAC_TX_STATUS_BIN + i * 4);

	printk("[atl1c] карта: предел кадра %u, приём %s, очередь приёма %s, передача %s/%s%s\n",
	       (unsigned)AT_R32(REG_MTU),
	       (mac & MAC_CTRL_RX_EN) ? "ВКЛ" : "ВЫКЛ",
	       (rxq & RXQ_CTRL_EN)    ? "ВКЛ" : "ВЫКЛ",
	       (mac & MAC_CTRL_TX_EN) ? "ВКЛ" : "ВЫКЛ",
	       (txq & TXQ_CTRL_EN)    ? "ВКЛ" : "ВЫКЛ",
	       (mac & MAC_CTRL_BC_EN) ? ", широковещание берёт" : ", ШИРОКОВЕЩАНИЕ НЕ БЕРЁТ");
	printk("[atl1c] MAC принял: %lu (широк %lu), больше предела %lu, FCS %lu, длина %lu,"
	       " переполнение FIFO %lu / кольца %lu, выравнивание %lu, не тот адрес %lu\n",
	       at_rx[RX_OK], at_rx[RX_BCAST], at_rx[RX_SZ_OV], at_rx[RX_FCS], at_rx[RX_LEN],
	       at_rx[RX_FIFO_OV], at_rx[RX_RRD_OV], at_rx[RX_ALIGN], at_rx[RX_FILTERED]);
	printk("[atl1c] MAC отдал в провод: %lu, обрезано по пределу %lu, опустошение %lu,"
	       " длина %lu, поздних столкновений %lu, брошено %lu\n",
	       at_tx[TX_OK], at_tx[TX_TRUNC], at_tx[TX_UNDERRUN], at_tx[TX_LEN_ERR],
	       at_tx[TX_LATE_COL], at_tx[TX_ABORT_COL]);
}

/* Задача-сторож: держит процесс живым и молча спит.
 *
 * Выглядит бесполезно, но она обязательна, и вот почему. Карта DMA'ит в НАШУ память: кольца
 * дескрипторов и буферы приёма выданы по DMA-праву этому процессу. Умри процесс — ядро вернёт
 * эти страницы в общий котёл и отдаст их кому-нибудь другому, а карта продолжит писать в них
 * принятые кадры. Получится порча чужой памяти, которая обнаружится далеко от причины и совсем
 * не в сетевом коде.
 *
 * Правильный выход из драйвера — остановить карту (`ndo_stop`), и он появится вместе со службой,
 * которая этим драйвером управляет. Пока такой службы нет, честнее не выходить вовсе.
 */
static void atl1c_keepalive(void *arg)
{
	(void)arg;
	for (;;)
		msleep(60000); /* холостой ход планировщика спит по-настоящему (Веха 134) */
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
		/* СНАЧАЛА ДОЖДАТЬСЯ ЛИНКА. Драйвер проверяет его РОВНО ОДИН РАЗ — внутри ndo_open
		 * (`atl1c_check_link_status` в `atl1c_up`). Если в тот миг автосогласование ещё идёт
		 * (а оно занимает секунды — на этой машине 4.8), драйвер честно решает «линка нет» и
		 * ВЫКЛЮЧАЕТ MAC. Повторить проверку потом некому: её зовёт обработчик прерывания по
		 * событию смены линка, а до рабочих прерываний мы ещё не дошли.
		 *
		 * Отсюда и загадка «кадры отдаются, карта их не берёт»: кольцо наполнялось, а
		 * передатчик был выключен (MAC_CTRL без TX_EN). В харнессе первого контакта (Веха 132)
		 * ожидание было — здесь я его потерял.
		 *
		 * Ждём ПУБЛИЧНОЙ функцией драйвера, не подглядывая в его внутренности. */
		{
			struct atl1c_adapter *ad = netdev_priv(ndev);
			unsigned waited;
			u16 bmsr = 0;

			for (waited = 0; waited < 15000; waited += 100) {
				atl1c_read_phy_reg(&ad->hw, MII_BMSR, &bmsr); /* бит залипающий */
				if (atl1c_read_phy_reg(&ad->hw, MII_BMSR, &bmsr))
					break;
				if ((bmsr & BMSR_LSTATUS) && (bmsr & BMSR_ANEGCOMPLETE))
					break;
				msleep(100);
			}
			printk("[atl1c] линк перед подъёмом: BMSR %04x через %u мс — %s\n",
			       bmsr, waited,
			       (bmsr & BMSR_LSTATUS) ? "ЕСТЬ" : "НЕТ (интерфейс поднимется без TX)");
		}

		printk("[atl1c] поднимаю интерфейс '%s' (ndo_open)\n", ndev->name);
		if (!ndev->netdev_ops || !ndev->netdev_ops->ndo_open) {
			printk("[atl1c] у драйвера нет ndo_open — это не сетевое устройство?\n");
			return;
		}
		/* Веха 195 — через `lx_netdev_open`, а не прямым `ndo_open`: кроме открытия он ставит
		 * `IFF_UP`. Без флага обработчик прерывания драйвера считает интерфейс выключенным и
		 * не принимает ни одного кадра (найдено на 8139too, причина у драйверов общая). */
		printk("[atl1c] --- вход в ndo_open ---\n");
		err = lx_netdev_open(ndev);
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

		/* Веха 195 — объявиться КАРТОЙ СИСТЕМЫ: принятые кадры уходят в ядро, а исходящие
		 * приходят оттуда, и над этой картой работает обычный `net-srv`. Проверено на RTL8139
		 * в QEMU (`ping` через неизменённый `8139too`); на живом AR8151 путь тот же, но на
		 * железе владельца ещё не гонялся — об этом сказано в заметке вехи, а не умолчано. */
		lx_netdev_attach(ndev);

		/* Веха 199.7 — пусть пульс спрашивает и саму карту (см. `atl1c_diag` выше). Ставим
		 * ПОСЛЕ подъёма: до `ndo_open` регистры ещё ничего не значат. */
		lx_net_set_diag(atl1c_diag);

		/* Дальше карта остаётся поднятой и обслуживает приём сама (NAPI по прерыванию). */
		lx_task_create(atl1c_keepalive, NULL, "atl1c-idle");
	}
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);
	uintptr_t dma_cap  = vsys_start_cap(1);
	uintptr_t irq_cap  = vsys_start_cap(2);
	uintptr_t ndev_cap = vsys_start_cap(3); /* Веха 195 — право БЫТЬ картой системы */

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
	lx_net_set_netdev_cap(ndev_cap);
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
