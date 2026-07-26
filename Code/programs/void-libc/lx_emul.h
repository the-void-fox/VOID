/* lx_emul.h — C-порт Linux-API шима (Веха 54) поверх фундамента userspace-драйверов.
 *
 * C-путь после Rust-каркаса (Веха 53): драйвер пишется на C «как в ядре Linux» (ioremap,
 * readl/writel, dma_alloc_coherent, kmalloc, request_irq, printk), а шим (lx_emul.c, вкомпилен
 * в libvoid.a) переводит вызовы в syscall'ы VOID. Заголовок не тянет syscall.h — драйвер НЕ
 * видит ни cap, ни ecall. Это шаг к Genode dde_linux: следующая цель — реальные .c из Linux.
 *
 * Имена — как в Linux, где безопасно (не сталкиваются с newlib): драйвер выглядит как ядровый.
 */
#pragma once
#include <stddef.h>
#include <stdint.h>

/* Устройство — ресурсы драйвера (как struct device/pci_dev, но несёт capability'и, сминченные
 * init'ом): mmio_cap→ioremap, dma_cap→dma_alloc_coherent, irq_cap→request_irq. */
struct lx_device {
    uintptr_t mmio_cap;
    uintptr_t dma_cap;
    uintptr_t irq_cap;
};

/* DMA-буфер: cpu — как видит его драйвер (VA), dma — физ-адрес для железа (dma_handle). */
struct lx_dma {
    void *cpu;
    uintptr_t dma;
};

/* «Права нет» в поле irq_cap (init мог не выдать IRQ-cap). */
#define LX_NO_CAP ((uintptr_t)-1)

/* Точка входа драйвера (роль module_init + матчинг шины probe): собрать device из стартовых прав
 * (slot 0/1/2 = mmio/dma/irq), поднять kit, звать probe; код возврата probe → код выхода. Не
 * возвращается. */
void lx_module_init(int (*probe)(struct lx_device *dev));

/* printk/pr_info — как в Linux, printf-стиль (через newlib vsnprintf → консоль ядра). */
void printk(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

/* ─── MMIO: ioremap + accessors (real Linux names, inline volatile) ─── */
void *ioremap(uintptr_t mmio_cap, size_t len);
void iounmap(void *addr);
static inline void writel(uint32_t v, void *a) { *(volatile uint32_t *)a = v; }
static inline void writew(uint16_t v, void *a) { *(volatile uint16_t *)a = v; }
static inline void writeb(uint8_t v, void *a) { *(volatile uint8_t *)a = v; }
static inline void writeq(uint64_t v, void *a) { *(volatile uint64_t *)a = v; }
static inline uint32_t readl(const void *a) { return *(volatile const uint32_t *)a; }
static inline uint16_t readw(const void *a) { return *(volatile const uint16_t *)a; }
static inline uint8_t readb(const void *a) { return *(volatile const uint8_t *)a; }

/* ─── DMA ─── */
struct lx_dma dma_alloc_coherent(uintptr_t dma_cap, size_t size);

/* ─── kmalloc/kfree (поверх кучи процесса) ─── */
void *kmalloc(size_t size);
void *kzalloc(size_t size);
void kfree(void *p);

/* ─── request_irq: threaded-oneshot IRQ через нить ─── */
int request_irq(uintptr_t irq_cap, void (*handler)(void));

/* ─── completion (futex): сигнал нить-IRQ → драйвер ─── */
struct completion {
    volatile uint32_t flag;
};
void init_completion(struct completion *c);
void complete(struct completion *c);
void wait_for_completion(struct completion *c);

/* ─── задержки ─── */
void mdelay(unsigned ms);
void udelay(unsigned us);
