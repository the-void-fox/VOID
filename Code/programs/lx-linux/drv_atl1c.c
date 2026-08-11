/* drv_atl1c.c — ПЕРВЫЙ КОНТАКТ с настоящей Atheros AR8151 через портированный код (Веха 132).
 *
 * То же, что Веха 69 сделала для e1000, только карта настоящая, а не эмулированная: в QEMU
 * AR8151 никто не эмулирует, поэтому каждая проверка — загрузка живого X54C. Отсюда устройство
 * харнесса: он не «шаг за шагом», а ОДИН прогон, отвечающий на максимум вопросов сразу. Печатаем
 * даже то, что вроде бы очевидно, — переспросить стоит перезагрузки ноутбука.
 *
 * Спавнится init'ом как userspace-драйвер (Вехи 51–54): start_cap 0 = MMIO-cap на BAR0.
 * DMA и прерывания тут не нужны — до колец дескрипторов дело дойдёт следующей вехой.
 *
 * Что проверяется ВЕНДОРНЫМ кодом (неизменённый atl1c_hw.c из Linux 6.18.7):
 *   - `atl1c_check_eeprom_exist` — есть ли EEPROM/флеш с настройками;
 *   - `atl1c_read_mac_addr`      — MAC карты (главное доказательство: адрес обязан совпасть с
 *                                  тем, что показывает Linux на этой же машине);
 *   - `atl1c_read_phy_reg`       — MDIO работает (идентификатор PHY и BMSR);
 *   - `atl1c_get_speed_and_duplex` — что PHY говорит о линке.
 *
 * Чего здесь СОЗНАТЕЛЬНО нет: `atl1c_reset_mac`, `atl1c_setup_mac_funcs` и `atl1c_get_mac_type`
 * объявлены `static` в atl1c_main.c и снаружи недоступны. Тип чипа приходится определять здесь
 * повторно — четыре строки, скопированные по смыслу (device id → nic_type). Это единственное
 * место, где мы дублируем логику драйвера, и оно отмечено нарочно: если Atheros когда-нибудь
 * поменяет соответствие, разойдётся именно тут.
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h>

#include "atl1c.h"
#include "lx_sched.h"

#include <linux/delay.h> /* msleep — уступает процессор в задаче Lx_kit */
#include <linux/mii.h>

/* Куда ляжет окно регистров карты. Тот же адрес, что у e1000-харнесса: пространство своё у
 * каждого процесса, и драйвер в системе один. */
#define ATL1C_BAR0_VA 0x50000000UL

/* Имя драйвера объявлено в atl1c.h и определено в atl1c_main.c — а его мы в этот харнесс не
 * линкуем (там весь netdev-обвяз, до которого дело дойдёт следующей вехой). Определяем сами,
 * тем же значением: `atl1c_hw.c` печатает его в диагностике спящего режима. */
char atl1c_driver_name[] = "atl1c";

static struct atl1c_hw      g_hw;
static struct pci_dev       g_pdev;
/* Вендорный код ходит через hw->adapter->pdev и смотрит adapter->msg_enable (гейты сообщений).
 * Полноценный adapter появится вместе с netdev; здесь нужны ровно эти два поля. */
static struct atl1c_adapter g_adapter;

/* Ответ `ffff` на шине MDIO значит НЕ «все биты подняты», а «никто не ответил»: незанятая линия
 * читается единицами. Без этой проверки харнесс бодро сообщал «линк ЕСТЬ, автосогласование
 * завершено» по регистру, которого никто не выставлял. */
static bool mdio_silent(u16 v) { return v == 0xffff; }

static void print_mac(const char *what, const u8 *mac)
{
	printk("[atl1c] %s = %02x:%02x:%02x:%02x:%02x:%02x\n", what,
	       mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
}

/* Тип чипа по device id. Копия `atl1c_get_mac_type` (она static — см. шапку). Для 0x1083
 * различие l1d_2 / mt решает магическое слово в регистре: у варианта MediaTek там 0xaabb1234. */
static enum atl1c_nic_type nic_type_of(u16 device_id, u8 __iomem *hw_addr)
{
	switch (device_id) {
	case PCI_DEVICE_ID_ATTANSIC_L2C:   return athr_l2c;
	case PCI_DEVICE_ID_ATTANSIC_L1C:   return athr_l1c;
	case PCI_DEVICE_ID_ATHEROS_L2C_B:  return athr_l2c_b;
	case PCI_DEVICE_ID_ATHEROS_L2C_B2: return athr_l2c_b2;
	case PCI_DEVICE_ID_ATHEROS_L1D:    return athr_l1d;
	case PCI_DEVICE_ID_ATHEROS_L1D_2_0:
		return readl(hw_addr + REG_MT_MAGIC) == MT_MAGIC ? athr_mt : athr_l1d_2;
	default:                           return athr_l1c;
	}
}

static const char *nic_type_name(enum atl1c_nic_type t)
{
	switch (t) {
	case athr_l1c:    return "AR8131 (L1C)";
	case athr_l2c:    return "AR8132 (L2C)";
	case athr_l2c_b:  return "AR8152 v1.1 (L2C_B)";
	case athr_l2c_b2: return "AR8152 v2.0 (L2C_B2)";
	case athr_l1d:    return "AR8151 v1.0 (L1D)";
	case athr_l1d_2:  return "AR8151 v2.0 (L1D_2)";
	case athr_mt:     return "MediaTek";
	default:          return "?";
	}
}

/* Работа с картой идёт ЗАДАЧЕЙ планировщика Lx_kit: вендорный код зовёт msleep, а тот уступает
 * процессор. Вне задачи уступать некому — и первая же пауза в PHY повисла бы. */
static void atl1c_task(void *arg)
{
	u32 reg;
	u16 phy_id1 = 0, phy_id2 = 0, bmsr = 0, speed = 0, duplex = 0;
	unsigned waited;
	int err;

	(void)arg;

	/* 1. Живая ли карта. Регистр мастер-управления содержит ревизию и идентификатор чипа;
	 *    сплошные единицы значат «окно отображено не туда» или «карта не отвечает». */
	reg = readl(g_hw.hw_addr + REG_MASTER_CTRL);
	printk("[atl1c] MASTER_CTRL = %08x\n", (unsigned)reg);
	if (reg == 0xffffffffu) {
		printk("[atl1c] карта не отвечает — окно регистров отображено не туда\n");
		return;
	}
	printk("[atl1c] LINK_CTRL   = %08x\n",
	       (unsigned)readl(g_hw.hw_addr + REG_LINK_CTRL));

	/* 2. Тип чипа. Ждём AR8151 v2.0 — именно её опознала опись PCI (Веха 130). */
	g_hw.nic_type = nic_type_of(g_hw.device_id, g_hw.hw_addr);
	printk("[atl1c] чип: %s (device id %04x, ревизия %02x)\n",
	       nic_type_name(g_hw.nic_type), g_hw.device_id, g_hw.revision_id);

	/* Настройки, которые в драйвере проставляет `atl1c_setup_mac_funcs` — она static (см. шапку).
	 * Значимо здесь одно: ATL1C_HIB_DISABLE НЕ выставлен, и по этому признаку `atl1c_phy_reset`
	 * включает PHY спящий режим ровно так же, как в Linux. Остальные флаги на путь PHY не влияют,
	 * но пусть состояние совпадает с настоящим — расхождение потом ищется дольше, чем пишется. */
	g_hw.ctrl_flags = ATL1C_INTR_MODRT_ENABLE | ATL1C_TXQ_MODE_ENHANCE |
			  ATL1C_ASPM_L0S_SUPPORT | ATL1C_ASPM_L1_SUPPORT |
			  ATL1C_ASPM_CTRL_MON;
	if (g_hw.nic_type == athr_l1c || g_hw.nic_type == athr_l1d ||
	    g_hw.nic_type == athr_l1d_2)
		g_hw.link_cap_flags |= ATL1C_LINK_CAP_1000M;

	/* 3. EEPROM. Его отсутствие не беда (MAC тогда берётся из регистров, зашитых прошивкой),
	 *    но знать это надо: от этого зависит, откуда взялся адрес. */
	err = atl1c_check_eeprom_exist(&g_hw);
	printk("[atl1c] EEPROM: %s\n", err ? "есть" : "нет");

	/* 4. MAC-адрес — ВЕНДОРНЫМ кодом. Это главная проверка прогона: совпадение с тем, что
	 *    показывает Linux на этой же машине, доказывает, что путь «наши шимы → чтение
	 *    регистров реальной карты» работает целиком. */
	err = atl1c_read_mac_addr(&g_hw);
	if (err) {
		printk("[atl1c] ПОСТОЯННОГО АДРЕСА НЕТ — драйвер выдумал случайный (err %d)\n", err);
	}
	print_mac(err ? "MAC (случайный!)" : "MAC (из карты)", g_hw.mac_addr);

	/* 5. ПОДНЯТЬ PHY. Первый прогон на живой карте показал `PHY id = ffff:ffff` — шина MDIO
	 *    молчала. Причина не в шине: после включения машины PHY стоит в power-down
	 *    (`GPHY_CTRL_PHY_IDDQ`) и под внешним сбросом, и выводит её оттуда именно эта функция.
	 *    Она вендорная — то есть последовательность подъёма мы не сочиняем, а исполняем ту же,
	 *    что Linux. */
	err = atl1c_phy_reset(&g_hw);
	printk("[atl1c] подъём PHY (atl1c_phy_reset): %s\n", err ? "ОШИБКА" : "ok");

	/* 6. MDIO. Идентификатор PHY у Atheros — 0x004d:0xd0xx. */
	if (atl1c_read_phy_reg(&g_hw, MII_PHYSID1, &phy_id1) ||
	    atl1c_read_phy_reg(&g_hw, MII_PHYSID2, &phy_id2)) {
		printk("[atl1c] MDIO: обмен не завершился — PHY не опросить\n");
		return;
	}
	if (mdio_silent(phy_id1) && mdio_silent(phy_id2)) {
		printk("[atl1c] PHY НЕ ОТВЕЧАЕТ (id ffff:ffff — линия свободна)\n");
		return;
	}
	printk("[atl1c] PHY id = %04x:%04x\n", phy_id1, phy_id2);

	/* BMSR сразу после сброса — только чтобы показать, что регистр читается осмысленно. Линка
	 * тут быть НЕ МОЖЕТ: согласование ещё не начиналось (его запустит шаг 7). */
	if (atl1c_read_phy_reg(&g_hw, MII_BMSR, &bmsr) || mdio_silent(bmsr)) {
		printk("[atl1c] BMSR не прочитан — PHY молчит\n");
		return;
	}
	printk("[atl1c] BMSR сразу после сброса = %04x (согласование ещё не начиналось)\n", bmsr);

	/* 7. ЗАПУСТИТЬ автосогласование. Прошлый прогон читал BMSR через микросекунды после сброса
	 *    PHY и сообщал «линк нет» при воткнутом кабеле — и был формально прав: согласование к
	 *    тому мигу не начиналось. Начинает его `atl1c_phy_init` (вендорная): она объявляет, что
	 *    мы умеем, и взводит ANRESTART. Что объявлять — берётся из `autoneg_advertised`, и это
	 *    ровно то, что ставит драйвер в своём `atl1c_sw_init`. */
	g_hw.media_type = MEDIA_TYPE_AUTO_SENSOR;
	g_hw.autoneg_advertised = ADVERTISED_Autoneg;
	err = atl1c_phy_init(&g_hw);
	printk("[atl1c] автосогласование запущено (atl1c_phy_init): %s\n", err ? "ОШИБКА" : "ok");
	if (err) {
		return;
	}

	/* 8. ДОЖДАТЬСЯ его. На витой паре согласование занимает секунды; `msleep` здесь уступает
	 *    процессор, а не крутит его вхолостую (задача Lx_kit).
	 *
	 *    Потолок 15 секунд, и это не перестраховка: первый удачный прогон на X54C сошёлся за
	 *    4800 мс при потолке в 5000 — то есть уложился с запасом в двести миллисекунд. Гигабит
	 *    согласуется дольше ста мегабит, а на другом коммутаторе бывает и дольше; истёкший
	 *    потолок выглядел бы как «карта не работает», и искали бы это в драйвере. */
	for (waited = 0; waited < 15000; waited += 100) {
		atl1c_read_phy_reg(&g_hw, MII_BMSR, &bmsr); /* залипающий бит — читаем дважды */
		if (atl1c_read_phy_reg(&g_hw, MII_BMSR, &bmsr))
			break;
		if (mdio_silent(bmsr))
			break;
		if (bmsr & BMSR_ANEGCOMPLETE)
			break;
		msleep(100);
	}
	printk("[atl1c] BMSR = %04x через %u мс — линк %s, автосогласование %s\n",
	       bmsr, waited,
	       (bmsr & BMSR_LSTATUS) ? "ЕСТЬ" : "нет",
	       (bmsr & BMSR_ANEGCOMPLETE) ? "завершено" : "НЕ завершено");

	/* 9. Что PHY говорит о скорости. Осмысленно только при поднятой несущей. */
	if (bmsr & BMSR_LSTATUS) {
		if (atl1c_get_speed_and_duplex(&g_hw, &speed, &duplex) == 0)
			printk("[atl1c] линк: %u Мбит/с, %s дуплекс\n",
			       speed, duplex ? "полный" : "полу");
		else
			printk("[atl1c] скорость/дуплекс не определились\n");
	} else {
		printk("[atl1c] несущей нет — кабель не воткнут либо согласование не сошлось\n");
	}

	printk("[atl1c] первый контакт завершён\n");
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);

	printk("[atl1c] харнесс первого контакта (Веха 132)\n");

	if (mmio_cap == VOID_NO_CAP) {
		printk("[atl1c] нет MMIO-права — запускать должен init\n");
		return 1;
	}
	if (!vsys_mmio_map(mmio_cap, ATL1C_BAR0_VA)) {
		printk("[atl1c] окно регистров не отобразилось\n");
		return 1;
	}

	/* Поля, которые в настоящем драйвере заполняет PCI-слой: у нас право на окно уже выдано
	 * ядром, а идентификаторы карты известны из описи шины (Веха 130). */
	memset(&g_pdev, 0, sizeof(g_pdev));
	g_pdev.vendor = PCI_VENDOR_ID_ATTANSIC;
	g_pdev.device = PCI_DEVICE_ID_ATHEROS_L1D_2_0;

	memset(&g_adapter, 0, sizeof(g_adapter));
	g_adapter.pdev = &g_pdev;
	g_adapter.msg_enable = 0xffff; /* говорить всё: прогон на железе стоит перезагрузки */

	memset(&g_hw, 0, sizeof(g_hw));
	g_hw.adapter = &g_adapter;
	g_hw.hw_addr = (u8 __iomem *)ATL1C_BAR0_VA;
	g_hw.device_id = g_pdev.device;
	g_hw.vendor_id = g_pdev.vendor;
	g_hw.hibernate = false;

	lx_task_create(atl1c_task, NULL, "atl1c");
	lx_sched_run();
	return 0;
}
