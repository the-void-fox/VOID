/* lx_emul.c — C-порт Linux-API шима (Веха 54). Вкомпилен в libvoid.a; драйверы, не зовущие
 * этих функций (hello/bzip2), его .o не тянут (статический архив). Перевод Linux-API → syscall'ы
 * VOID (фундамент Вех 51–52): ioremap→SYS_MMIO_MAP, dma_alloc_coherent→SYS_DMA_ALLOC,
 * request_irq→нить(SYS_THREAD_SPAWN)+SYS_IRQ_WAIT (= Linux threaded-oneshot IRQ), completion→futex,
 * kmalloc→куча процесса, printk→SYS_WRITE. Однопроцессорно, аллокатор — newlib (зовётся из main).
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "lx_emul.h"
#include "syscall.h"

/* ─── раскладка простора драйвера (USER-регион, ниже кучи 0x6000_0000) ─── */
#define IOREMAP_BASE 0x50000000UL
#define IOREMAP_STEP 0x01000000UL /* 16 МиБ на устройство */
#define DMA_BASE 0x58000000UL
#define DMA_STEP 0x1000UL /* 4 КиБ на когерентную страницу */
#define IRQ_STACK (32u * 1024u)

static uintptr_t ioremap_next = IOREMAP_BASE;
static uintptr_t dma_next = DMA_BASE;

/* ─── printk ─── */
void printk(const char *fmt, ...) {
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    if (n < 0)
        return;
    if ((size_t)n >= sizeof(buf))
        n = sizeof(buf) - 1;
    vsys_write(buf, (size_t)n);
}

/* ─── driver-model ─── */
void lx_module_init(int (*probe)(struct lx_device *)) {
    struct lx_device dev;
    dev.mmio_cap = vsys_start_cap(0);
    dev.dma_cap = vsys_start_cap(1);
    dev.irq_cap = vsys_start_cap(2);
    int rc = probe(&dev);
    vsys_exit(rc == 0 ? 0 : 1);
}

/* ─── MMIO ─── */
void *ioremap(uintptr_t mmio_cap, size_t len) {
    (void)len; /* окно даёт ядро по cap целиком */
    uintptr_t va = ioremap_next;
    ioremap_next += IOREMAP_STEP;
    if (!vsys_mmio_map(mmio_cap, va))
        return NULL;
    return (void *)va;
}
void iounmap(void *addr) { (void)addr; } /* окно живёт до выхода процесса */

/* ─── DMA ─── */
struct lx_dma dma_alloc_coherent(uintptr_t dma_cap, size_t size) {
    (void)size; /* одна страница (≤4 КиБ: кольца/буферы демо влезают) */
    struct lx_dma d;
    uintptr_t va = dma_next;
    dma_next += DMA_STEP;
    uintptr_t pa = vsys_dma_alloc(dma_cap, va);
    if (pa == VOID_NO_CAP) {
        d.cpu = NULL;
        d.dma = 0;
    } else {
        d.cpu = (void *)va;
        d.dma = pa;
    }
    return d;
}

/* ─── kmalloc/kfree: куча процесса (newlib malloc, зовётся из main) ─── */
void *kmalloc(size_t size) { return malloc(size); }
void *kzalloc(size_t size) { return calloc(1, size); }
void kfree(void *p) { free(p); }

/* ─── request_irq: threaded-oneshot IRQ через нить ─── */
struct irq_ctx {
    uintptr_t irq_cap;
    void (*handler)(void);
};

/* Тело нити: крутит irq_wait (взводит oneshot-линию, усыпляет до прерывания) и зовёт handler.
 * Это threaded-oneshot IRQ Linux: обработчик в контексте нити. Отказ irq_wait (нет права) —
 * нить завершается (процесс живёт). Не возвращается. */
static void irq_thread(uintptr_t arg) {
    struct irq_ctx *ctx = (struct irq_ctx *)arg;
    for (;;) {
        if (!vsys_irq_wait(ctx->irq_cap))
            vsys_thread_exit(1);
        ctx->handler();
    }
}

int request_irq(uintptr_t irq_cap, void (*handler)(void)) {
    void *stack = kmalloc(IRQ_STACK);
    struct irq_ctx *ctx = (struct irq_ctx *)kmalloc(sizeof(*ctx));
    if (!stack || !ctx)
        return -1;
    ctx->irq_cap = irq_cap;
    ctx->handler = handler;
    uintptr_t stack_top = (uintptr_t)stack + IRQ_STACK;
    if (vsys_thread_spawn((uintptr_t)irq_thread, (uintptr_t)ctx, stack_top) == VOID_NO_CAP)
        return -1;
    return 0;
}

/* ─── completion (futex) ─── */
void init_completion(struct completion *c) { c->flag = 0; }
void complete(struct completion *c) {
    c->flag = 1;
    vsys_futex_wake((const uint32_t *)&c->flag, 1);
}
void wait_for_completion(struct completion *c) {
    while (c->flag == 0)
        vsys_futex_wait((const uint32_t *)&c->flag, 0, 0);
}

/* ─── задержки ─── */
void udelay(unsigned us) {
    uint64_t ticks = ((uint64_t)us * 1000) / VOID_TICK_NS; /* мкс → нс → тики */
    uint64_t start = vsys_ticks();
    while (vsys_ticks() - start < ticks) {
    }
}
void mdelay(unsigned ms) { udelay(ms * 1000); }
