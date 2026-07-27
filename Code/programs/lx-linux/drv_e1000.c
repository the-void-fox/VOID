/* drv_e1000.c — портированный e1000 на НАСТОЯЩЕМ QEMU-e1000:
 *   Веха 69 — MMIO (регистры/MAC реальной карты),
 *   Веха 70 — DMA-TX (передача кадра),
 *   Веха 71 — DMA-RX + ARP round-trip (шлём ARP-запрос шлюзу, принимаем ARP-ответ, ОПРОСОМ DD),
 *   Веха 72 — RX по ПРЕРЫВАНИЮ: ISR + request_irq↔vsys_irq_wait (start_cap 2), без опроса,
 *   Веха 73 — PING: ARP узнаёт MAC шлюза → ICMP echo request → приём echo reply (по прерыванию).
 *
 * Спавнится init'ом как userspace-драйвер (Вехи 51–54): start_cap 0 = MMIO-cap, 1 = DMA-cap,
 * 2 = IRQ-cap. main() маппит BAR0, отдаёт DMA/IRQ-cap в lx_net.c, запускает планировщик; работа с
 * картой — в задаче (vendored msleep уступает). Кольца дескрипторов строит VENDORED
 * e1000_setup_all_[tx|rx]_resources на РЕАЛЬНОМ DMA; движки TX/RX конфигурируем регистрами (vendored
 * e1000_configure_[tx|rx] static). IRQ: включаем RX-причины в IMS, регистрируем ISR через request_irq
 * (шим заводит его в планировщике Lx_kit) — планировщик в idle спит на vsys_irq_wait, по прерыванию
 * карты зовёт ISR, тот будит задачу через completion. Сам ICMP/ARP/IP собираем здесь (стек — net-srv,
 * мост к нему дальше); это ПРЯМОЙ ping от портированного драйвера. x86 (на riscv e1000 нет).
 */
#include <stdio.h>
#include <string.h>

#include <syscall.h>

#include "e1000.h"
#include "lx_sched.h"

#include <linux/completion.h> /* completion — ISR будит задачу (Веха 72) */
#include <linux/interrupt.h>  /* irqreturn_t / IRQ_HANDLED / request_irq */

extern void lx_net_set_dma_cap(uintptr_t cap);
extern void lx_net_set_irq_cap(uintptr_t cap);

#define E1000_BAR0_VA 0x50000000UL

/* Наши сетевые параметры (SLIRP: гость 10.0.2.15, шлюз 10.0.2.2). */
static const u8 OUR_IP[4] = { 10, 0, 2, 15 };
static const u8 GW_IP[4]  = { 10, 0, 2, 2 };

static struct e1000_adapter g_adapter;
static struct e1000_tx_ring g_tx_ring;
static struct e1000_rx_ring g_rx_ring;
static struct pci_dev       g_pdev;

static struct completion    g_rx_done;   /* ISR будит bring-up-задачу по RX-прерыванию (Веха 72) */
static volatile u32         g_isr_icr;   /* ICR последнего обслуженного прерывания (для отчёта) */
static volatile unsigned    g_isr_count; /* сколько прерываний карты обслужили */

#define RX_AVAIL 4 /* сколько RX-дескрипторов отдаём карте (с буферами) */

/* Обработчик прерывания e1000 (Веха 72). Планировщик зовёт его в softirq-контексте
 * (sched_current == NULL) по возврату из vsys_irq_wait. Чтение ICR (R/clr) гасит причину и снимает
 * INTx карты; при RX-причине будим задачу через completion. */
static irqreturn_t e1000_isr(int irq, void *dev)
{
	struct e1000_hw *hw = &g_adapter.hw;
	u32 icr = er32(ICR);
	(void)irq; (void)dev;
	if (!icr)
		return IRQ_NONE;   /* не наше прерывание (INT_ASSERTED сброшен) */
	g_isr_icr = icr;
	g_isr_count++;
	if (icr & (E1000_ICR_RXT0 | E1000_ICR_RXDMT0 | E1000_ICR_RXO))
		complete(&g_rx_done); /* пришёл кадр — разбудить bring-up-задачу */
	return IRQ_HANDLED;
}

/* Собрать ARP-запрос «who has GW_IP» в buf (60 Б). Вернуть длину. */
static unsigned build_arp_request(u8 *buf, const u8 *our_mac)
{
	memset(buf, 0, 60);
	memset(buf, 0xff, 6);                 /* eth dst = broadcast */
	memcpy(buf + 6, our_mac, 6);          /* eth src */
	buf[12] = 0x08; buf[13] = 0x06;       /* ethertype ARP */
	buf[14] = 0x00; buf[15] = 0x01;       /* htype ethernet */
	buf[16] = 0x08; buf[17] = 0x00;       /* ptype IPv4 */
	buf[18] = 6; buf[19] = 4;             /* hlen/plen */
	buf[20] = 0x00; buf[21] = 0x01;       /* oper = request */
	memcpy(buf + 22, our_mac, 6);         /* sha */
	memcpy(buf + 28, OUR_IP, 4);          /* spa */
	/* tha = 0 */
	memcpy(buf + 38, GW_IP, 4);           /* tpa = шлюз */
	return 60;
}

/* Контрольная сумма интернет (RFC 1071): 16-битные слова в сетевом порядке, one's complement.
 * Возвращает значение в хост-порядке; в пакет кладём big-endian (старший байт, младший байт). */
static u16 inet_csum(const u8 *data, unsigned len)
{
	u32 sum = 0;
	while (len > 1) { sum += ((u32)data[0] << 8) | data[1]; data += 2; len -= 2; }
	if (len) sum += (u32)data[0] << 8;
	while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
	return (u16)~sum;
}

/* Собрать ICMP echo request (ping) к GW_IP: Ethernet(dst=MAC шлюза) + IPv4 + ICMP + 32 Б payload.
 * Вернуть длину кадра (>= 60). */
static unsigned build_icmp_echo(u8 *buf, const u8 *our_mac, const u8 *gw_mac, u16 id, u16 seq)
{
	u8 *ip = buf + 14, *icmp = buf + 34;
	const unsigned payload = 32, icmp_len = 8 + payload, ip_len = 20 + icmp_len, frame = 14 + ip_len;
	unsigned i;
	u16 c;

	memset(buf, 0, frame);
	/* Ethernet */
	memcpy(buf, gw_mac, 6);
	memcpy(buf + 6, our_mac, 6);
	buf[12] = 0x08; buf[13] = 0x00;              /* ethertype IPv4 */
	/* IPv4 */
	ip[0] = 0x45;                                /* версия 4, IHL 5 */
	ip[2] = ip_len >> 8; ip[3] = ip_len & 0xff;  /* total length */
	ip[6] = 0x40;                                /* флаги: DF */
	ip[8] = 64;                                  /* TTL */
	ip[9] = 1;                                   /* proto ICMP */
	memcpy(ip + 12, OUR_IP, 4);
	memcpy(ip + 16, GW_IP, 4);
	c = inet_csum(ip, 20); ip[10] = c >> 8; ip[11] = c & 0xff;
	/* ICMP echo request */
	icmp[0] = 8;                                 /* type: echo request */
	icmp[4] = id >> 8; icmp[5] = id & 0xff;
	icmp[6] = seq >> 8; icmp[7] = seq & 0xff;
	for (i = 0; i < payload; i++) icmp[8 + i] = (u8)i;
	c = inet_csum(icmp, icmp_len); icmp[2] = c >> 8; icmp[3] = c & 0xff;
	return frame;
}

/* Передать кадр frame_dma длиной len дескриптором idx TX-кольца; ждать DD. Вернуть DD (0/1). */
static int e1000_tx(struct e1000_hw *hw, struct e1000_tx_desc *tx, unsigned idx,
                    dma_addr_t frame_dma, unsigned len)
{
	unsigned i;
	tx[idx].buffer_addr = cpu_to_le64(frame_dma);
	tx[idx].lower.data  = cpu_to_le32(len | E1000_TXD_CMD_EOP | E1000_TXD_CMD_IFCS | E1000_TXD_CMD_RS);
	tx[idx].upper.data  = 0;
	E1000_WRITE_FLUSH();
	ew32(TDT, idx + 1);
	for (i = 0; i < 2000; i++) { if (le32_to_cpu(tx[idx].upper.data) & E1000_TXD_STAT_DD) break; udelay(50); }
	return (le32_to_cpu(tx[idx].upper.data) & E1000_TXD_STAT_DD) ? 1 : 0;
}

/* Дождаться следующего принятого дескриптора ПО ПРЕРЫВАНИЮ (Веха 72), сканируя с *from.
 * Карта даёт и пороговое прерывание (RXDMT0) без кадра — ждём в цикле, пока не появится DD.
 * Возвращает индекс дескриптора; *from сдвигает за него (следующий приём — дальше по кольцу). */
static unsigned e1000_wait_rx(struct e1000_rx_desc *rx, unsigned *from)
{
	unsigned got = -1u, d;
	do {
		wait_for_completion(&g_rx_done);
		reinit_completion(&g_rx_done);
		for (d = *from; d < RX_AVAIL; d++)
			if (rx[d].status & E1000_RXD_STAT_DD) { got = d; break; }
	} while (got == -1u);
	*from = got + 1;
	return got;
}

static void e1000_bringup(void *arg)
{
	struct e1000_hw *hw = &g_adapter.hw;
	struct e1000_tx_desc *tx;
	struct e1000_rx_desc *rx;
	dma_addr_t frame_dma = 0, rxbuf_dma[RX_AVAIL];
	u8 *frame, *rxbuf[RX_AVAIL];
	u8 gw_mac[6];
	u16 speed = 0, duplex = 0;
	u32 status, ral, rah;
	unsigned len, i, got, rx_scan = 0;
	int ok;
	s32 r;
	(void)arg;

	/* ── (1) bring-up (Веха 69) ── */
	hw->vendor_id = 0x8086;
	hw->device_id = E1000_DEV_ID_82540EM;
	e1000_set_mac_type(hw);
	e1000_reset_hw(hw);
	e1000_init_eeprom_params(hw);
	e1000_read_mac_addr(hw);
	e1000_get_speed_and_duplex(hw, &speed, &duplex);
	status = readl(hw->hw_addr + E1000_STATUS);
	printf("[e1000] MAC=%02x:%02x:%02x:%02x:%02x:%02x STATUS=0x%08x (link %s) speed=%d\n",
	       hw->mac_addr[0], hw->mac_addr[1], hw->mac_addr[2], hw->mac_addr[3],
	       hw->mac_addr[4], hw->mac_addr[5], status, (status & E1000_STATUS_LU) ? "UP" : "down", (int)speed);

	/* ── (2) кольца TX+RX на реальном DMA (VENDORED setup) ── */
	g_adapter.pdev = &g_pdev;
	g_adapter.num_tx_queues = 1; g_adapter.tx_ring = &g_tx_ring; g_tx_ring.count = E1000_DEFAULT_TXD;
	g_adapter.num_rx_queues = 1; g_adapter.rx_ring = &g_rx_ring; g_rx_ring.count = E1000_DEFAULT_RXD;
	r  = e1000_setup_all_tx_resources(&g_adapter);
	r |= e1000_setup_all_rx_resources(&g_adapter);
	printf("[e1000] vendored setup TX/RX-колец на DMA → r=%d (tx dma=0x%llx, rx dma=0x%llx)\n",
	       (int)r, (unsigned long long)g_tx_ring.dma, (unsigned long long)g_rx_ring.dma);
	if (r || !g_tx_ring.desc || !g_rx_ring.desc) { printf("[e1000] нет колец — стоп\n"); fflush(stdout); vsys_exit(1); }

	/* Линк вверх + приёмный адрес (RAL0/RAH0) = наш MAC. */
	ew32(CTRL, er32(CTRL) | E1000_CTRL_SLU);
	ral = hw->mac_addr[0] | (hw->mac_addr[1] << 8) | (hw->mac_addr[2] << 16) | (hw->mac_addr[3] << 24);
	rah = hw->mac_addr[4] | (hw->mac_addr[5] << 8) | E1000_RAH_AV;
	writel(ral, hw->hw_addr + E1000_RA);
	writel(rah, hw->hw_addr + E1000_RA + 4);

	/* TX-движок. */
	ew32(TDBAL, (u32)(g_tx_ring.dma & 0xffffffffULL));
	ew32(TDBAH, (u32)(g_tx_ring.dma >> 32));
	ew32(TDLEN, g_tx_ring.count * (u32)sizeof(struct e1000_tx_desc));
	ew32(TDH, 0); ew32(TDT, 0);
	ew32(TCTL, E1000_TCTL_EN | E1000_TCTL_PSP | (0x0f << 4) | (0x40 << 12));
	ew32(TIPG, 0x0060200a);

	/* RX-буферы (по странице на дескриптор) + RX-движок (промиск: примем и уникаст-ответ). */
	rx = (struct e1000_rx_desc *)g_rx_ring.desc;
	for (i = 0; i < RX_AVAIL; i++) {
		rxbuf[i] = dma_alloc_coherent(&g_pdev.dev, 4096, &rxbuf_dma[i], 0);
		if (!rxbuf[i]) { printf("[e1000] нет RX-буфера %u — стоп\n", i); fflush(stdout); vsys_exit(1); }
		rx[i].buffer_addr = cpu_to_le64(rxbuf_dma[i]);
		rx[i].status = 0;
	}
	ew32(RDBAL, (u32)(g_rx_ring.dma & 0xffffffffULL));
	ew32(RDBAH, (u32)(g_rx_ring.dma >> 32));
	ew32(RDLEN, g_rx_ring.count * (u32)sizeof(struct e1000_rx_desc));
	ew32(RDH, 0); ew32(RDT, RX_AVAIL); /* карте доступны дескрипторы 0..RX_AVAIL-1 */
	ew32(RCTL, E1000_RCTL_EN | E1000_RCTL_BAM | E1000_RCTL_UPE | E1000_RCTL_MPE | E1000_RCTL_SECRC);
	E1000_WRITE_FLUSH();

	/* ── IRQ (Веха 72): регистрируем ISR и включаем RX-причины в IMS. Дальше приём — по прерыванию
	 * (не опрос DD): планировщик уснёт на vsys_irq_wait, ARP-ответ поднимет RXT0 → INTx → ISR. ─ */
	init_completion(&g_rx_done);
	request_irq(0, e1000_isr, 0, "e1000", &g_adapter);
	ew32(IMS, E1000_IMS_RXT0 | E1000_IMS_RXDMT0 | E1000_IMS_RXO);
	E1000_WRITE_FLUSH();

	tx = (struct e1000_tx_desc *)g_tx_ring.desc;
	frame = dma_alloc_coherent(&g_pdev.dev, 4096, &frame_dma, 0);
	if (!frame) { printf("[e1000] нет TX-буфера — стоп\n"); fflush(stdout); vsys_exit(1); }

	/* ── (3) ARP: узнаём MAC шлюза (TX запрос → RX ответ по прерыванию) ── */
	len = build_arp_request(frame, hw->mac_addr);
	ok = e1000_tx(hw, tx, 0, frame_dma, len);
	printf("[e1000] TX ARP-запрос «who has %d.%d.%d.%d» → DD=%d\n",
	       GW_IP[0], GW_IP[1], GW_IP[2], GW_IP[3], ok);
	got = e1000_wait_rx(rx, &rx_scan);
	{
		u8 *p = rxbuf[got];
		if (!(p[12] == 0x08 && p[13] == 0x06 && p[20] == 0x00 && p[21] == 0x02)) {
			printf("[e1000] RX: ожидали ARP-ответ, пришло ethertype=%02x%02x — стоп\n", p[12], p[13]);
			fflush(stdout); vsys_exit(1);
		}
		memcpy(gw_mac, p + 22, 6); /* sha = MAC шлюза */
		printf("[e1000] ARP-ОТВЕТ: %d.%d.%d.%d = %02x:%02x:%02x:%02x:%02x:%02x (обслужено IRQ=%u, ICR=0x%08x)\n",
		       p[28], p[29], p[30], p[31], gw_mac[0], gw_mac[1], gw_mac[2], gw_mac[3], gw_mac[4], gw_mac[5],
		       g_isr_count, g_isr_icr);
	}

	/* ── (4) PING: ICMP echo request шлюзу → echo reply по прерыванию (Веха 73) ── */
	len = build_icmp_echo(frame, hw->mac_addr, gw_mac, 0x1234, 1);
	ok = e1000_tx(hw, tx, 1, frame_dma, len);
	printf("[e1000] TX ICMP echo request → %d.%d.%d.%d (%u Б) → DD=%d\n",
	       GW_IP[0], GW_IP[1], GW_IP[2], GW_IP[3], len, ok);

	/* Ждём echo reply; пропускаем чужие кадры (напр. повторный ARP), пока не кончатся дескрипторы. */
	got = -1u;
	while (rx_scan < RX_AVAIL) {
		unsigned d = e1000_wait_rx(rx, &rx_scan);
		u8 *p = rxbuf[d];
		if (p[12] == 0x08 && p[13] == 0x00 && p[23] == 1 &&      /* IPv4 + proto ICMP */
		    p[34] == 0 &&                                        /* ICMP type = echo reply */
		    p[26] == GW_IP[0] && p[27] == GW_IP[1] && p[28] == GW_IP[2] && p[29] == GW_IP[3]) {
			got = d; break;
		}
		printf("[e1000] RX: дескриптор %u — не echo reply (ethertype=%02x%02x), ждём дальше\n",
		       d, p[12], p[13]);
	}

	if (got == -1u) {
		printf("[e1000] PING: echo reply не пришёл — FAIL\n");
		fflush(stdout); vsys_exit(1);
	}
	{
		u8 *p = rxbuf[got];
		u16 seq = ((u16)p[40] << 8) | p[41]; /* ICMP seq: frame off 34+6..7 */
		printf("[e1000] ICMP ECHO REPLY от %d.%d.%d.%d: id=0x%02x%02x seq=%u ttl=%u (обслужено IRQ=%u)\n",
		       p[26], p[27], p[28], p[29], p[38], p[39], seq, p[22], g_isr_count);
		printf("[e1000] Результат: PING %d.%d.%d.%d через НЕИЗМЕНЁННЫЙ e1000 (DMA+IRQ, ARP+ICMP) — OK\n",
		       GW_IP[0], GW_IP[1], GW_IP[2], GW_IP[3]);
	}
	fflush(stdout);
	vsys_exit(0);
}

int main(void)
{
	uintptr_t mmio_cap = vsys_start_cap(0);
	uintptr_t dma_cap  = vsys_start_cap(1);
	uintptr_t irq_cap  = vsys_start_cap(2);

	printf("== Портированный e1000: PING шлюза (ARP+ICMP, DMA+IRQ) на РЕАЛЬНОМ QEMU-e1000 (Веха 73) ==\n");
	fflush(stdout);
	if (mmio_cap == VOID_NO_CAP) { printf("[e1000] нет MMIO-cap — выход\n"); fflush(stdout); return 0; }
	if (!vsys_mmio_map(mmio_cap, E1000_BAR0_VA)) { printf("[e1000] vsys_mmio_map отказал\n"); fflush(stdout); return 1; }
	g_adapter.hw.hw_addr = (u8 *)(uintptr_t)E1000_BAR0_VA;
	lx_net_set_dma_cap(dma_cap);
	lx_net_set_irq_cap(irq_cap);
	printf("[e1000] BAR0 по MMIO-cap → VA 0x%lx; DMA-cap %s; IRQ-cap %s\n",
	       (unsigned long)E1000_BAR0_VA, (dma_cap == VOID_NO_CAP) ? "НЕТ" : "есть",
	       (irq_cap == VOID_NO_CAP) ? "НЕТ" : "есть");
	fflush(stdout);

	lx_task_create(e1000_bringup, NULL, "e1000drv");
	lx_sched_run();
	return 0;
}
