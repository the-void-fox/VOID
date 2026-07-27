/* linux/interrupt.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 *
 * Регистрация обработчика прерывания. На VOID реальный IRQ приходит через SYS_IRQ_WAIT (Веха 52,
 * [[irq]]): request_irq заводит задачу-обработчик Lx_kit, которая ждёт IRQ-cap и зовёт handler.
 * Тела — в lx_kit.c. tasklet/softirq сведены на нашу отложенную работу. */
#ifndef _LINUX_INTERRUPT_H_SHIM
#define _LINUX_INTERRUPT_H_SHIM

#include <linux/types.h>
#include <linux/atomic.h>

typedef enum irqreturn {
	IRQ_NONE        = 0,
	IRQ_HANDLED     = 1,
	IRQ_WAKE_THREAD = 2,
} irqreturn_t;

typedef irqreturn_t (*irq_handler_t)(int irq, void *dev_id);

#define IRQF_SHARED     0x00000080
#define IRQF_PROBE_SHARED 0x00000100
#define IRQF_NO_THREAD  0x00010000

int  request_irq(unsigned int irq, irq_handler_t handler, unsigned long flags,
		 const char *name, void *dev);
void free_irq(unsigned int irq, void *dev);
void disable_irq(unsigned int irq);
void enable_irq(unsigned int irq);
void synchronize_irq(unsigned int irq);

/* tasklet — отложенное «мягкое прерывание»; у нас на задаче-воркере (тела в lx_kit.c). */
struct tasklet_struct {
	void (*func)(unsigned long data);
	unsigned long data;
	unsigned long state;
};
void tasklet_init(struct tasklet_struct *t, void (*func)(unsigned long), unsigned long data);
void tasklet_schedule(struct tasklet_struct *t);
void tasklet_kill(struct tasklet_struct *t);

#endif /* _LINUX_INTERRUPT_H_SHIM */
