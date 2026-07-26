/* lx_e1000.c — Intel e1000 как Linux-СТИЛЕВОЙ драйвер на C поверх шима lx_emul (Веха 54, C-путь).
 *
 * То же, что Rust-драйвер lx_e1000 (Веха 53), но на C: карту трогает Linux-подобным API
 * (ioremap/readl/writel, dma_alloc_coherent, request_irq threaded, wait_for_completion, probe),
 * ни строчки о cap/syscall'ах. Собирается кросс-gcc против void-libc (nix/default.nix), едет на
 * диск мостом, init спавнит его на карту e1000. Шаг к dde_linux: дальше — реальные .c из Linux.
 */
#include <stdint.h>

#include <lx_emul.h>

/* Регистры e1000 (смещения в BAR0). */
#define CTRL 0x0000
#define ICR 0x00c0
#define ICS 0x00c8
#define IMS 0x00d0
#define IMC 0x00d8
#define TCTL 0x0400
#define TIPG 0x0410
#define TDBAL 0x3800
#define TDBAH 0x3804
#define TDLEN 0x3808
#define TDH 0x3810
#define TDT 0x3818
#define RAL 0x5400
#define RAH 0x5404

#define CTRL_RST (1u << 26)
#define CTRL_SLU (1u << 6)
#define CTRL_ASDE (1u << 5)
#define ICR_LSC (1u << 2)
#define TCTL_EN (1u << 1)
#define TCTL_PSP (1u << 3)
#define DESC_DD (1u << 0)
#define TX_EOP (1u << 0)
#define TX_IFCS (1u << 1)
#define TX_RS (1u << 3)

static uint8_t *g_mmio;         /* база регистров — читает нить-обработчик IRQ */
static struct completion g_done; /* сигнал «прерывание обработано» из нити-IRQ в probe */

/* Threaded-обработчик прерывания (в нити request_irq): прочитать ICR (снимает линию),
 * напечатать причину, снова замаскировать, сигналить probe через completion. */
static void e1000_irq(void) {
    uint32_t cause = readl(g_mmio + ICR);
    printk("[lx_e1000-c] IRQ! threaded-обработчик разбужен, ICR=0x%08x -- lx_emul(C): request_irq РАБОТАЕТ\n",
           cause);
    writel(0xffffffffu, g_mmio + IMC); /* замаскировать причины — уходим */
    complete(&g_done);
}

/* probe — как у Linux-драйвера: поднять устройство из ресурсов dev. Всё железо — через шим. */
static int probe(struct lx_device *dev) {
    uint8_t *mmio = (uint8_t *)ioremap(dev->mmio_cap, 0x20000);
    if (!mmio) {
        printk("[lx_e1000-c] ioremap отказал (нет MMIO-cap?)\n");
        return 1;
    }
    g_mmio = mmio;
    init_completion(&g_done);

    /* Сброс, маскировка прерываний, линк вверх. */
    writel(0xffffffffu, mmio + IMC);
    writel(readl(mmio + CTRL) | CTRL_RST, mmio + CTRL);
    for (int i = 0; i < 1000000; i++)
        if ((readl(mmio + CTRL) & CTRL_RST) == 0)
            break;
    writel(0xffffffffu, mmio + IMC);
    writel(readl(mmio + CTRL) | CTRL_SLU | CTRL_ASDE, mmio + CTRL);

    /* MAC из RAL/RAH. */
    uint32_t ral = readl(mmio + RAL), rah = readl(mmio + RAH);
    uint8_t mac[6] = { ral, ral >> 8, ral >> 16, ral >> 24, rah, rah >> 8 };
    printk("[lx_e1000-c] драйвер поднят на lx_emul (C), MAC %02x:%02x:%02x:%02x:%02x:%02x\n",
           mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    /* DMA: кольцо TX + буфер кадра. */
    struct lx_dma ring = dma_alloc_coherent(dev->dma_cap, 4096);
    struct lx_dma buf = dma_alloc_coherent(dev->dma_cap, 4096);
    if (!ring.cpu || !buf.cpu) {
        printk("[lx_e1000-c] dma_alloc_coherent отказал (нет DMA-cap?)\n");
        return 1;
    }

    /* TX-кольцо (8 дескрипторов = 128 Б); дескриптор 0. */
    writel((uint32_t)ring.dma, mmio + TDBAL);
    writel((uint32_t)((uint64_t)ring.dma >> 32), mmio + TDBAH);
    writel(128, mmio + TDLEN);
    writel(0, mmio + TDH);
    writel(0, mmio + TDT);
    writel(TCTL_EN | TCTL_PSP | (0x0f << 4) | (0x40 << 12), mmio + TCTL);
    writel(0x0060200au, mmio + TIPG);

    /* 60-байтный broadcast-кадр в DMA-буфере. */
    uint8_t *b = (uint8_t *)buf.cpu;
    for (int i = 0; i < 6; i++) {
        writeb(0xff, b + i);
        writeb(mac[i], b + 6 + i);
    }
    writeb(0x88, b + 12); /* ethertype 0x88B5 */
    writeb(0xb5, b + 13);
    for (int i = 14; i < 60; i++)
        writeb('V', b + i);

    /* Дескриптор 0: адрес буфера, длина, cmd; двинуть TDT. */
    uint8_t *d = (uint8_t *)ring.cpu;
    writeq((uint64_t)buf.dma, d);
    writew(60, d + 8);
    writeb(TX_EOP | TX_IFCS | TX_RS, d + 11);
    writeb(0, d + 12);
    writel(1, mmio + TDT);

    /* Опрос DD — карта прочитала DMA-дескриптор и буфер. */
    int done = 0;
    for (int i = 0; i < 10000000; i++)
        if (readb(d + 12) & DESC_DD) {
            done = 1;
            break;
        }
    printk(done ? "[lx_e1000-c] TX: DD выставлен -- MMIO+DMA через lx_emul(C) РАБОТАЮТ\n"
                : "[lx_e1000-c] TX: DD не выставлен (таймаут)\n");

    /* Прерывание через request_irq (threaded) ВМЕСТО опроса. */
    if (dev->irq_cap == LX_NO_CAP) {
        printk("[lx_e1000-c] IRQ-cap не выдан -- прерывание пропущено (демо на опросе)\n");
        return 0;
    }
    if (request_irq(dev->irq_cap, e1000_irq) != 0) {
        printk("[lx_e1000-c] request_irq отказал\n");
        return 1;
    }
    writel(0xffffffffu, mmio + ICR); /* сбросить залипшие причины */
    (void)readl(mmio + ICR);
    writel(ICR_LSC, mmio + IMS); /* размаскировать Link-Status-Change */
    writel(ICR_LSC, mmio + ICS); /* инициировать её — карта поднимет линию */
    printk("[lx_e1000-c] request_irq + жду прерывание (wait_for_completion, опрос выключен)...\n");
    wait_for_completion(&g_done);
    return 0;
}

int main(void) {
    lx_module_init(probe);
    return 0; /* недостижимо: lx_module_init делает vsys_exit */
}
