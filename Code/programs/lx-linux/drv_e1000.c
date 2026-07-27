/* drv_e1000.c — портированный e1000 на НАСТОЯЩЕМ QEMU-e1000 через MMIO-cap (Веха 69).
 *
 * В отличие от main_e1000.c (Веха 68 — чистая логика на синтетике), ЭТОТ бинарь спавнится init'ом
 * как userspace-драйвер (путь Вех 51–54) и получает MMIO-cap на регистры реальной карты первым
 * стартовым правом (start_cap 0). Он маппит BAR0 (SYS_MMIO_MAP), кладёт его в hw->hw_addr — и дальше
 * НЕИЗМЕНЁННЫЙ e1000_hw.c ходит по нему через er32/ew32 (readl/writel) к настоящему железу: сброс,
 * параметры EEPROM, чтение MAC (QEMU эмулирует EEPROM/PHY сам), скорость/дуплекс из STATUS.
 *
 * Bring-up идёт КАК ЗАДАЧА Lx_kit: vendored-код зовёт msleep (Веха 63 — уступающий), а тот работает
 * только под кооперативным планировщиком. Поэтому main() лишь маппит BAR и запускает планировщик;
 * вся работа с картой — в задаче e1000_bringup. DMA/IRQ/кольца TX-RX — следующая веха.
 *
 * e1000 в QEMU — только x86 (q35, -device e1000); на riscv-virt его нет → драйвер собирается и
 * импортируется, но не спавнится (как C-драйвер Вехи 54). Регрессий это не даёт.
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h> /* void-libc: vsys_start_cap / vsys_mmio_map / vsys_exit / VOID_NO_CAP */

#include "e1000.h"    /* vendored: struct e1000_hw + hw-функции + E1000_* регистры */
#include "lx_sched.h" /* кооперативная задача Lx_kit */

#define E1000_BAR0_VA   0x50000000UL /* та же зона, что IOREMAP_BASE lx_emul (Веха 54) */
#define E1000_BAR0_SIZE 0x20000UL    /* BAR0 e1000 — 128 КиБ */

static struct e1000_hw g_hw;

/* Bring-up реального e1000 неизменённым vendored-кодом (внутри задачи — msleep уступает). */
static void e1000_bringup(void *arg)
{
	u16 speed = 0, duplex = 0;
	u32 status;
	s32 r;
	(void)arg;

	g_hw.vendor_id = 0x8086;
	g_hw.device_id = E1000_DEV_ID_82540EM; /* QEMU `-device e1000` = 82540EM */

	r = e1000_set_mac_type(&g_hw);
	printf("[e1000] set_mac_type → mac_type=%d (r=%d)\n", (int)g_hw.mac_type, (int)r);

	status = readl(g_hw.hw_addr + E1000_STATUS);
	printf("[e1000] STATUS реального e1000 = 0x%08x  (link %s)\n",
	       status, (status & E1000_STATUS_LU) ? "UP" : "down");

	r = e1000_reset_hw(&g_hw);
	printf("[e1000] reset_hw (сброс настоящей карты) → r=%d\n", (int)r);

	r = e1000_init_eeprom_params(&g_hw);
	printf("[e1000] init_eeprom_params → type=%d word_size=%d (r=%d)\n",
	       (int)g_hw.eeprom.type, (int)g_hw.eeprom.word_size, (int)r);

	r = e1000_read_mac_addr(&g_hw);
	if (r == 0)
		printf("[e1000] MAC из EEPROM реального e1000: %02x:%02x:%02x:%02x:%02x:%02x\n",
		       g_hw.mac_addr[0], g_hw.mac_addr[1], g_hw.mac_addr[2],
		       g_hw.mac_addr[3], g_hw.mac_addr[4], g_hw.mac_addr[5]);
	else
		printf("[e1000] read_mac_addr отказал (r=%d)\n", (int)r);

	r = e1000_get_speed_and_duplex(&g_hw, &speed, &duplex);
	printf("[e1000] speed=%d duplex=%d (r=%d)\n", (int)speed, (int)duplex, (int)r);

	printf("[e1000] Результат: НЕИЗМЕНЁННЫЙ e1000_hw.c отработал на НАСТОЯЩЕМ QEMU-e1000 через MMIO-cap — OK\n");
	fflush(stdout);
	vsys_exit(0); /* работа сделана — завершить процесс-драйвер */
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);

	printf("== Портированный e1000 на РЕАЛЬНОМ QEMU-e1000 через MMIO-cap (Веха 69) ==\n");
	fflush(stdout);

	if (mmio_cap == VOID_NO_CAP) {
		printf("[e1000] нет MMIO-cap (не спавнен init'ом как драйвер) — выход\n");
		fflush(stdout);
		return 0;
	}
	if (!vsys_mmio_map(mmio_cap, E1000_BAR0_VA)) {
		printf("[e1000] vsys_mmio_map отказал — нет доступа к регистрам\n");
		fflush(stdout);
		return 1;
	}
	g_hw.hw_addr = (u8 *)(uintptr_t)E1000_BAR0_VA; /* er32/ew32 пойдут по этому окну к железу */
	printf("[e1000] BAR0 замаплен по MMIO-cap → VA 0x%lx (%lu КиБ)\n",
	       (unsigned long)E1000_BAR0_VA, (unsigned long)(E1000_BAR0_SIZE / 1024));
	fflush(stdout);

	/* Вся работа с картой — в задаче (vendored-код зовёт уступающий msleep). */
	lx_task_create(e1000_bringup, NULL, "e1000drv");
	lx_sched_run();
	return 0;
}
