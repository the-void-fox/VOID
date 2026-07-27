/* main_e1000.c — харнесс НЕИЗМЕНЁННОГО драйвера Intel e1000 на VOID (Веха 68).
 *
 * Здесь линкуются РЕАЛЬНЫЕ файлы ядра Linux 6.18.7 (drivers/net/ethernet/intel/e1000/e1000_hw.c,
 * e1000_main.c, e1000_param.c) — verbatim, GPL-2.0 — против наших шим-заголовков «linux/…» и
 * рантайма Lx_kit (lx_kit.c) + сетевых заглушек (lx_net.c). Демонстрируем, что vendored-логика
 * драйвера ИСПОЛНЯЕТСЯ на VOID: зовём чистые функции e1000_hw.c (без опроса железа) —
 * e1000_set_mac_type (device_id→тип чипа) и e1000_set_media_type. Обе арх.
 *
 * Полный запуск probe/open/TX/RX против настоящего QEMU-e1000 по MMIO/DMA/IRQ-cap — следующая веха.
 */
#include <stdio.h>
#include <string.h>

#include "e1000.h"

/* Синтетические «регистры» устройства (окно BAR0). er32/ew32 vendored-кода бьют СЮДА через
 * OS-adaptation слой (e1000_osdep.h: er32→readl(hw->hw_addr+reg)). Настоящее окно даст ioremap
 * по MMIO-cap VOID на следующей вехе; здесь — буфер, чтобы чтение регистра не падало. */
static u8 e1000_regs[0x10000];

int main(void)
{
	struct e1000_hw hw;
	s32 r;
	int ok;

	printf("== Неизменённый Linux-драйвер e1000 на VOID (Веха 68) ==\n");
	printf("   (vendored e1000_hw.c/e1000_main.c/e1000_param.c 6.18.7 + шимы + Lx_kit)\n");

	/* Синтетический чип 8086:100E (82540EM — реальный e1000) с окном регистров. */
	memset(&hw, 0, sizeof(hw));
	hw.vendor_id = 0x8086;
	hw.device_id = E1000_DEV_ID_82540EM;
	hw.hw_addr   = e1000_regs; /* er32/ew32 работают по этому адресу */

	/* Чистая логика vendored-кода: опознание типа MAC по device_id. */
	r = e1000_set_mac_type(&hw);
	printf("e1000_set_mac_type(device=0x%04x) → ret=%d, mac_type=%d (ждём %d=e1000_82540)\n",
	       hw.device_id, r, (int)hw.mac_type, (int)e1000_82540);

	/* Тип среды: vendored-код ЧИТАЕТ регистр STATUS через er32→readl (OS-adaptation слой). */
	e1000_set_media_type(&hw);
	printf("e1000_set_media_type → media_type=%d (0=copper; читал STATUS через er32→readl)\n",
	       (int)hw.media_type);

	ok = (r == 0 && hw.mac_type == e1000_82540 && hw.media_type == e1000_media_type_copper);
	printf("Результат: mac_type=%d media=%d — %s\n",
	       (int)hw.mac_type, (int)hw.media_type, ok ? "OK" : "FAIL");
	return 0;
}
