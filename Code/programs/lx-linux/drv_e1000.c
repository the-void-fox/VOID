/* drv_e1000.c — портированный e1000 на НАСТОЯЩЕМ QEMU-e1000: MMIO (Веха 69) + DMA-TX (Веха 70).
 *
 * Спавнится init'ом как userspace-драйвер (путь Вех 51–54): start_cap 0 = MMIO-cap на регистры,
 * start_cap 1 = DMA-cap. main() маппит BAR0 (SYS_MMIO_MAP), отдаёт DMA-cap в lx_net.c и запускает
 * планировщик; вся работа с картой — в задаче (vendored msleep уступает, Веха 63).
 *
 * Задача: (1) bring-up неизменённым e1000_hw.c — set_mac_type/reset_hw/init_eeprom/read_mac (Веха 69);
 * (2) TX — vendored `e1000_setup_all_tx_resources` строит кольцо дескрипторов на РЕАЛЬНОМ DMA
 * (dma_alloc_coherent→DMA-cap), затем конфигурируем TX-движок (TDBAL/TDLEN/TCTL/TIPG — вручную, т.к.
 * vendored e1000_configure_tx static) и ПЕРЕДАЁМ кадр: дескриптор → TDT → ждём DD-бит (карта
 * вынесла кадр DMA'ом наружу). RX/IRQ/NAPI и мост к net-srv — следующая веха. x86 (на riscv e1000 нет).
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h> /* vsys_start_cap / vsys_mmio_map / vsys_exit / VOID_NO_CAP */

#include "e1000.h"    /* vendored: e1000_hw/e1000_adapter/e1000_tx_ring/e1000_tx_desc + функции */
#include "lx_sched.h" /* кооперативная задача Lx_kit */

extern void lx_net_set_dma_cap(uintptr_t cap); /* отдать DMA-cap в lx_net.c (dma_alloc_coherent) */

#define E1000_BAR0_VA   0x50000000UL
#define E1000_BAR0_SIZE 0x20000UL

static struct e1000_adapter g_adapter;   /* минимально заполненный — под vendored TX-setup */
static struct e1000_tx_ring g_tx_ring;   /* adapter->tx_ring[0] */
static struct pci_dev       g_pdev;      /* adapter->pdev (для dma_alloc_coherent(&pdev->dev,…)) */

/* Собрать 60-байтовый широковещательный кадр (содержимое неважно — проверяем сам факт передачи). */
static unsigned lx_build_frame(u8 *buf, const u8 *src_mac)
{
	memset(buf, 0, 60);
	memset(buf, 0xff, 6);        /* dst = broadcast */
	memcpy(buf + 6, src_mac, 6); /* src = наш MAC */
	buf[12] = 0x08; buf[13] = 0x00; /* ethertype IPv4 (заглушка) */
	buf[14] = 0x45;              /* немного «полезной нагрузки» */
	return 60;
}

static void e1000_bringup(void *arg)
{
	struct e1000_hw *hw = &g_adapter.hw;
	struct e1000_tx_desc *tx;
	u16 speed = 0, duplex = 0;
	dma_addr_t frame_dma = 0;
	u8 *frame;
	u32 status, tctl;
	unsigned len, i;
	s32 r;
	(void)arg;

	/* ── (1) bring-up на реальном железе (Веха 69) ── */
	hw->vendor_id = 0x8086;
	hw->device_id = E1000_DEV_ID_82540EM; /* QEMU `-device e1000` */

	r = e1000_set_mac_type(hw);
	status = readl(hw->hw_addr + E1000_STATUS);
	printf("[e1000] set_mac_type→mac_type=%d; STATUS реального e1000=0x%08x (link %s)\n",
	       (int)hw->mac_type, status, (status & E1000_STATUS_LU) ? "UP" : "down");

	e1000_reset_hw(hw);
	e1000_init_eeprom_params(hw);
	r = e1000_read_mac_addr(hw);
	printf("[e1000] MAC из EEPROM: %02x:%02x:%02x:%02x:%02x:%02x (r=%d)\n",
	       hw->mac_addr[0], hw->mac_addr[1], hw->mac_addr[2],
	       hw->mac_addr[3], hw->mac_addr[4], hw->mac_addr[5], (int)r);
	e1000_get_speed_and_duplex(hw, &speed, &duplex);
	printf("[e1000] speed=%d duplex=%d\n", (int)speed, (int)duplex);

	/* ── (2) TX: vendored-кольцо на реальном DMA + передача кадра ── */
	g_adapter.pdev          = &g_pdev;
	g_adapter.num_tx_queues = 1;
	g_adapter.tx_ring       = &g_tx_ring;
	g_tx_ring.count         = E1000_DEFAULT_TXD; /* 256 дескрипторов = 4 КиБ = 1 DMA-страница */

	r = e1000_setup_all_tx_resources(&g_adapter); /* VENDORED: кольцо через dma_alloc_coherent */
	printf("[e1000] e1000_setup_all_tx_resources (vendored, DMA-кольцо) → r=%d, ring dma=0x%llx\n",
	       (int)r, (unsigned long long)g_tx_ring.dma);
	if (r || !g_tx_ring.desc) { printf("[e1000] нет TX-кольца — стоп\n"); fflush(stdout); vsys_exit(1); }

	/* Поднять линк + сконфигурировать TX-движок (e1000_configure_tx static — делаем те же записи). */
	ew32(CTRL, er32(CTRL) | E1000_CTRL_SLU);
	ew32(TDBAL, (u32)(g_tx_ring.dma & 0xffffffffULL));
	ew32(TDBAH, (u32)(g_tx_ring.dma >> 32));
	ew32(TDLEN, g_tx_ring.count * (u32)sizeof(struct e1000_tx_desc));
	ew32(TDH, 0);
	ew32(TDT, 0);
	tctl = E1000_TCTL_EN | E1000_TCTL_PSP | (0x0f << 4) | (0x40 << 12); /* CT=15, COLD=64 (full) */
	ew32(TCTL, tctl);
	ew32(TIPG, 0x0060200a);
	E1000_WRITE_FLUSH();

	/* Кадр в DMA-память, дескриптор 0, звоним в дверь (TDT=1). */
	frame = dma_alloc_coherent(&g_pdev.dev, 4096, &frame_dma, 0);
	if (!frame) { printf("[e1000] нет DMA-буфера кадра — стоп\n"); fflush(stdout); vsys_exit(1); }
	len = lx_build_frame(frame, hw->mac_addr);

	tx = (struct e1000_tx_desc *)g_tx_ring.desc;
	tx[0].buffer_addr = cpu_to_le64(frame_dma);
	tx[0].lower.data  = cpu_to_le32(len | E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS | E1000_TXD_CMD_RS);
	tx[0].upper.data  = 0;
	E1000_WRITE_FLUSH();
	ew32(TDT, 1); /* дверной звонок — карта забирает дескриптор 0 */

	/* Ждём DD (карта вынесла кадр DMA'ом наружу и отписала статус). */
	for (i = 0; i < 2000; i++) {
		if (le32_to_cpu(tx[0].upper.data) & E1000_TXD_STAT_DD)
			break;
		udelay(50);
	}
	status = le32_to_cpu(tx[0].upper.data);
	printf("[e1000] TX: desc0.status=0x%02x DD=%d, TDH=%u TDT=%u (кадр %u Б передан по DMA)\n",
	       (unsigned)(status & 0xff), (status & E1000_TXD_STAT_DD) ? 1 : 0,
	       (unsigned)er32(TDH), (unsigned)er32(TDT), len);

	printf("[e1000] Результат: НЕИЗМЕНЁННЫЙ e1000 передал кадр на РЕАЛЬНОМ QEMU-e1000 (DMA-cap) — %s\n",
	       (status & E1000_TXD_STAT_DD) ? "OK" : "FAIL");
	fflush(stdout);
	vsys_exit(0);
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);
	uintptr_t dma_cap  = vsys_start_cap(1);

	printf("== Портированный e1000: MMIO + DMA-TX на РЕАЛЬНОМ QEMU-e1000 (Веха 70) ==\n");
	fflush(stdout);

	if (mmio_cap == VOID_NO_CAP) {
		printf("[e1000] нет MMIO-cap (не спавнен init'ом как драйвер) — выход\n");
		fflush(stdout);
		return 0;
	}
	if (!vsys_mmio_map(mmio_cap, E1000_BAR0_VA)) {
		printf("[e1000] vsys_mmio_map отказал\n");
		fflush(stdout);
		return 1;
	}
	g_adapter.hw.hw_addr = (u8 *)(uintptr_t)E1000_BAR0_VA;
	lx_net_set_dma_cap(dma_cap); /* dma_alloc_coherent в lx_net.c пойдёт по DMA-cap */
	printf("[e1000] BAR0 по MMIO-cap → VA 0x%lx; DMA-cap %s\n",
	       (unsigned long)E1000_BAR0_VA, (dma_cap == VOID_NO_CAP) ? "НЕТ" : "есть");
	fflush(stdout);

	lx_task_create(e1000_bringup, NULL, "e1000drv");
	lx_sched_run();
	return 0;
}
