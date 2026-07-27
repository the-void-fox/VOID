/* linux/device.h — ШИМ lx_emul (Веха 66), НЕ исходник Linux.
 *
 * Костяк driver-model ядра: `struct device` (узел железа), `struct device_driver` (драйвер),
 * `struct bus_type` (шина с функцией match). Регистрация связывает устройство с драйвером по
 * правилу шины и вызывает `.probe` (упрощённый `drivers/base/dd.c`). На этом стоит PCI: `pci_dev`
 * встраивает `device`, `pci_driver` — `device_driver`, `pci_register_driver` крутит ту же связку.
 * Здесь же `dev_*`-логирование (→ printk) и `dev_get/set_drvdata`.
 */
#ifndef _LINUX_DEVICE_H_SHIM
#define _LINUX_DEVICE_H_SHIM

#include <linux/printk.h>
#include <linux/types.h>

struct device;
struct device_driver;

struct bus_type {
	const char *name;
	int (*match)(struct device *dev, struct device_driver *drv);
	int (*probe)(struct device *dev);
};

struct device_driver {
	const char       *name;
	struct bus_type  *bus;
	struct module    *owner;
	int  (*probe)(struct device *dev);
	void (*remove)(struct device *dev); /* ядро 6.x: remove возвращает void */
	const void       *of_match_table;
	const void       *pm; /* dev_pm_ops — управление питанием (у нас не трогаем) */
	/* приватная линковка Lx_kit (список зарегистрированных драйверов) */
	struct device_driver *lx_next;
};

struct device {
	const char           *init_name;
	struct device        *parent;
	struct bus_type      *bus;
	struct device_driver *driver;
	void                 *driver_data;
	void                 *platform_data;
	const void           *of_node;
	void                (*release)(struct device *dev);
	u64                   coherent_dma_mask;
	u64                  *dma_mask;
	/* приватная линковка Lx_kit (список зарегистрированных устройств) */
	struct device        *lx_next;
};

static inline void *dev_get_drvdata(const struct device *dev) { return dev->driver_data; }
static inline void  dev_set_drvdata(struct device *dev, void *data) { dev->driver_data = data; }
static inline const char *dev_name(const struct device *dev)
{
	return dev->init_name ? dev->init_name : "(dev)";
}
#define dev_set_name(dev, fmt, ...) ((void)0) /* имя ставим напрямую в init_name */

/* Логирование устройства → printk (уровень/устройство пока не печатаем). */
#define dev_printk(level, dev, fmt, ...) printk(fmt, ##__VA_ARGS__)
#define dev_info(dev, fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define dev_err(dev, fmt, ...)    printk(fmt, ##__VA_ARGS__)
#define dev_warn(dev, fmt, ...)   printk(fmt, ##__VA_ARGS__)
#define dev_notice(dev, fmt, ...) printk(fmt, ##__VA_ARGS__)
#define dev_dbg(dev, fmt, ...)    printk(fmt, ##__VA_ARGS__)
#define dev_err_once(dev, fmt, ...) printk(fmt, ##__VA_ARGS__)
#define dev_warn_once(dev, fmt, ...) printk(fmt, ##__VA_ARGS__)

/* Управление питанием/пробуждением устройства (учётные). */
int  device_set_wakeup_enable(struct device *dev, bool enable);
int  device_wakeup_enable(struct device *dev);

/* dev_pm_ops + DEFINE_SIMPLE_DEV_PM_OPS: у нас питанием не управляем, но символ ops нужен как
 * цель `.driver.pm = pm_sleep_ptr(&ops)` (pm_sleep_ptr отдаёт NULL). Ссылаемся на suspend/resume,
 * чтобы они не были «unused». */
struct dev_pm_ops {
	int (*suspend)(struct device *dev);
	int (*resume)(struct device *dev);
	int (*freeze)(struct device *dev);
	int (*thaw)(struct device *dev);
	int (*poweroff)(struct device *dev);
	int (*restore)(struct device *dev);
};
#define DEFINE_SIMPLE_DEV_PM_OPS(name, suspend_fn, resume_fn) \
	const struct dev_pm_ops __attribute__((unused)) name = { \
		.suspend = suspend_fn, .resume = resume_fn, \
		.freeze = suspend_fn, .thaw = resume_fn, \
		.poweroff = suspend_fn, .restore = resume_fn, \
	}

/* Регистрация (тела в lx_kit.c): match по шине → bind → probe. */
int  bus_register(struct bus_type *bus);
void bus_unregister(struct bus_type *bus);
int  driver_register(struct device_driver *drv);
void driver_unregister(struct device_driver *drv);
int  device_register(struct device *dev);
void device_unregister(struct device *dev);
int  device_add(struct device *dev);
void device_del(struct device *dev);

#endif /* _LINUX_DEVICE_H_SHIM */
