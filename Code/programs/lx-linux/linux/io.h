/* linux/io.h — ШИМ lx_emul (Веха 60), НЕ исходник Linux.
 *
 * Доступ к регистрам устройства через память (MMIO): readl/writel и родня — типизированные
 * volatile-обращения фиксированной ширины (8/16/32/64). Драйвер читает/пишет регистры ИМЕННО так.
 * У нас те же имена/семантика поверх volatile-указателя.
 *
 * ПРО ioremap: в этом «вычислительном» мире устройств нет — ioremap отдаёт адрес как есть
 * (identity). НАСТОЯЩЕЕ отображение окна регистров даёт lx_emul по MMIO-capability (SYS_MMIO_MAP,
 * Вехи 51/54, [[userspace-drivers]]/[[lx-emul-c]]) — с ним io.h сведём, когда мир порта встретится
 * с драйверным. Здесь важны сами аксессоры: их и проверяет харнесс на буфере-«регистрах».
 */
#ifndef _LINUX_IO_H_SHIM
#define _LINUX_IO_H_SHIM

#include <linux/types.h>

typedef unsigned long phys_addr_t;
typedef phys_addr_t   resource_size_t;

/* Чтение регистра фиксированной ширины (LE-семантика на наших LE-архах). */
static inline u8  readb(const volatile void *addr) { return *(const volatile u8  *)addr; }
static inline u16 readw(const volatile void *addr) { return *(const volatile u16 *)addr; }
static inline u32 readl(const volatile void *addr) { return *(const volatile u32 *)addr; }
static inline u64 readq(const volatile void *addr) { return *(const volatile u64 *)addr; }

static inline void writeb(u8  val, volatile void *addr) { *(volatile u8  *)addr = val; }
static inline void writew(u16 val, volatile void *addr) { *(volatile u16 *)addr = val; }
static inline void writel(u32 val, volatile void *addr) { *(volatile u32 *)addr = val; }
static inline void writeq(u64 val, volatile void *addr) { *(volatile u64 *)addr = val; }

/* _relaxed — без барьеров упорядочивания; у нас те же обращения (одно ядро). */
#define readb_relaxed(a)     readb(a)
#define readw_relaxed(a)     readw(a)
#define readl_relaxed(a)     readl(a)
#define readq_relaxed(a)     readq(a)
#define writeb_relaxed(v, a) writeb((v), (a))
#define writew_relaxed(v, a) writew((v), (a))
#define writel_relaxed(v, a) writel((v), (a))
#define writeq_relaxed(v, a) writeq((v), (a))

/* Порт-подобные ioreadN/iowriteN поверх тех же MMIO-аксессоров. */
static inline u8  ioread8(const volatile void *a)  { return readb(a); }
static inline u16 ioread16(const volatile void *a) { return readw(a); }
static inline u32 ioread32(const volatile void *a) { return readl(a); }
static inline void iowrite8(u8 v, volatile void *a)   { writeb(v, a); }
static inline void iowrite16(u16 v, volatile void *a) { writew(v, a); }
static inline void iowrite32(u32 v, volatile void *a) { writel(v, a); }

/* identity — см. врезку про ioremap выше; настоящее окно даёт lx_emul по capability. */
static inline void *ioremap(phys_addr_t phys, size_t size) { (void)size; return (void *)phys; }
static inline void iounmap(volatile void *addr) { (void)addr; }

#endif /* _LINUX_IO_H_SHIM */
