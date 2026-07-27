/* linux/pci.h — ШИМ lx_emul (Веха 67), НЕ исходник Linux.
 *
 * Слой PCI поверх driver-model (Веха 66): `pci_dev` ВСТРАИВАЕТ `struct device`, `pci_driver` —
 * `struct device_driver`, `pci_register_driver` крутит ту же связку driver_register→match→probe,
 * но match идёт по таблице `id_table` (vendor/device). Конфиг-пространство и BAR'ы живут в pci_dev;
 * pci_enable_device/set_master/… — учётные (реальное окно регистров даёт ioremap по MMIO-cap VOID,
 * Вехи 51/54). Полигон — e1000 (drivers/net/ethernet/intel/e1000): его PCI-поверхность собрана здесь.
 */
#ifndef _LINUX_PCI_H_SHIM
#define _LINUX_PCI_H_SHIM

#include <linux/types.h>
#include <linux/device.h>
#include <linux/ioport.h>
#include <linux/io.h>

typedef unsigned long kernel_ulong_t;

/* ─ запись таблицы опознания устройства драйвером (в ядре — mod_devicetable.h) ─ */
struct pci_device_id {
	__u32 vendor, device;       /* Vendor/Device ID или PCI_ANY_ID */
	__u32 subvendor, subdevice; /* Subsystem ID или PCI_ANY_ID */
	__u32 class, class_mask;    /* (class,subclass,prog-if) — не сверяем */
	kernel_ulong_t driver_data; /* приватные данные драйвера */
	__u32 override_only;
};

#define PCI_ANY_ID (~0)

/* Заполнить vendor/device записи id_table (subsystem — «любой»). */
#define PCI_DEVICE(vend, dev) \
	.vendor = (vend), .device = (dev), \
	.subvendor = PCI_ANY_ID, .subdevice = PCI_ANY_ID
#define PCI_VDEVICE(vend, dev) \
	.vendor = PCI_VENDOR_ID_##vend, .device = (dev), \
	.subvendor = PCI_ANY_ID, .subdevice = PCI_ANY_ID

/* Вендоры, нужные полигону. */
#define PCI_VENDOR_ID_INTEL  0x8086
#define PCI_VENDOR_ID_VMWARE 0x15ad

/* Число стандартных BAR'ов заголовка Type 0. */
#define PCI_STD_NUM_BARS 6

/* Смещения конфиг-пространства (в ядре — uapi/linux/pci_regs.h). */
#define PCI_VENDOR_ID           0x00
#define PCI_DEVICE_ID           0x02
#define PCI_COMMAND             0x04
#define PCI_COMMAND_IO          0x1
#define PCI_COMMAND_MEMORY      0x2
#define PCI_COMMAND_MASTER      0x4
#define PCI_COMMAND_INVALIDATE  0x10
#define PCI_STATUS              0x06
#define PCI_REVISION_ID         0x08
#define PCI_SUBSYSTEM_VENDOR_ID 0x2c
#define PCI_SUBSYSTEM_ID        0x2e

/* Состояния питания (linux/pci.h). */
typedef int pci_power_t;
#define PCI_D0     ((pci_power_t)0)
#define PCI_D1     ((pci_power_t)1)
#define PCI_D2     ((pci_power_t)2)
#define PCI_D3hot  ((pci_power_t)3)
#define PCI_D3cold ((pci_power_t)4)

/* Каналы/результаты AER — нужны err_handler'у драйвера. */
typedef unsigned int pci_channel_state_t;
enum pci_channel_state {
	pci_channel_io_normal       = 1,
	pci_channel_io_frozen       = 2,
	pci_channel_io_perm_failure = 3,
};

typedef unsigned int pci_ers_result_t;
enum pci_ers_result {
	PCI_ERS_RESULT_NONE        = 1,
	PCI_ERS_RESULT_CAN_RECOVER = 2,
	PCI_ERS_RESULT_NEED_RESET  = 3,
	PCI_ERS_RESULT_DISCONNECT  = 4,
	PCI_ERS_RESULT_RECOVERED   = 5,
};

struct pci_dev;

struct pci_error_handlers {
	pci_ers_result_t (*error_detected)(struct pci_dev *dev, pci_channel_state_t state);
	pci_ers_result_t (*mmio_enabled)(struct pci_dev *dev);
	pci_ers_result_t (*slot_reset)(struct pci_dev *dev);
	void (*resume)(struct pci_dev *dev);
};

/* ─ устройство на шине PCI: ВСТРАИВАЕТ базовый узел driver-model ─ */
struct pci_dev {
	struct device dev;          /* база (Веха 66): bus/driver/drvdata живут здесь */
	u16 vendor;
	u16 device;
	u16 subsystem_vendor;
	u16 subsystem_device;
	u8  revision;
	unsigned int irq;
	unsigned int devfn;
	struct resource resource[PCI_STD_NUM_BARS]; /* BAR'ы (окна регистров/портов) */
	u8  lx_config[64];          /* конфиг-пространство (заголовок Type 0) */
	const struct pci_device_id *lx_id;     /* совпавшая запись id_table (для probe) */
	struct pci_driver          *lx_driver; /* связанный драйвер */
	char lx_name[16];           /* строковое имя ("0000:00:03.0") */
};

/* ─ драйвер PCI: ВСТРАИВАЕТ базовый драйвер driver-model ─ */
struct pci_driver {
	const char *name;
	const struct pci_device_id *id_table;
	int  (*probe)(struct pci_dev *dev, const struct pci_device_id *id);
	void (*remove)(struct pci_dev *dev);
	void (*shutdown)(struct pci_dev *dev);
	const struct pci_error_handlers *err_handler;
	struct device_driver driver; /* база (Веха 66): name/bus/probe/remove проставляет pci_register_driver */
};

/* Спуски между встроенной базой и объемлющей PCI-структурой. */
static inline struct pci_dev *to_pci_dev(struct device *d)
{
	return (struct pci_dev *)((char *)d - offsetof(struct pci_dev, dev));
}
static inline struct pci_driver *to_pci_driver(struct device_driver *drv)
{
	return (struct pci_driver *)((char *)drv - offsetof(struct pci_driver, driver));
}

/* Без CONFIG_PM_SLEEP управление питанием выключено → указатель на ops = NULL. */
#define pm_sleep_ptr(p) (NULL)

/* ─ жизненный цикл драйвера (тела в lx_kit.c): регистрация над driver-model ─ */
int  pci_register_driver(struct pci_driver *drv);
void pci_unregister_driver(struct pci_driver *drv);

/* ─ включение / DMA-мастер / write-invalidate (учётные над конфиг-словом COMMAND) ─ */
int  pci_enable_device(struct pci_dev *dev);
int  pci_enable_device_mem(struct pci_dev *dev);
void pci_disable_device(struct pci_dev *dev);
void pci_set_master(struct pci_dev *dev);
int  pci_set_mwi(struct pci_dev *dev);
void pci_clear_mwi(struct pci_dev *dev);

/* ─ выбор/резервирование BAR-регионов ─ */
int  pci_select_bars(struct pci_dev *dev, unsigned long flags);
int  pci_request_selected_regions(struct pci_dev *dev, int bars, const char *name);
void pci_release_selected_regions(struct pci_dev *dev, int bars);
void __iomem *pci_ioremap_bar(struct pci_dev *dev, int bar);

/* Аксессоры BAR (linux/pci.h — inline поверх resource[]). */
static inline resource_size_t pci_resource_start(const struct pci_dev *dev, int bar)
{ return dev->resource[bar].start; }
static inline resource_size_t pci_resource_end(const struct pci_dev *dev, int bar)
{ return dev->resource[bar].end; }
static inline resource_size_t pci_resource_len(const struct pci_dev *dev, int bar)
{
	if (dev->resource[bar].start == 0 && dev->resource[bar].end == 0)
		return 0;
	return resource_size(&dev->resource[bar]);
}
static inline unsigned long pci_resource_flags(const struct pci_dev *dev, int bar)
{ return dev->resource[bar].flags; }

/* ─ конфиг-пространство (LE над lx_config[]) ─ */
int pci_read_config_byte(struct pci_dev *dev, int where, u8 *val);
int pci_read_config_word(struct pci_dev *dev, int where, u16 *val);
int pci_read_config_dword(struct pci_dev *dev, int where, u32 *val);
int pci_write_config_byte(struct pci_dev *dev, int where, u8 val);
int pci_write_config_word(struct pci_dev *dev, int where, u16 val);
int pci_write_config_dword(struct pci_dev *dev, int where, u32 val);

/* ─ питание / пробуждение / сохранение состояния (учётные) ─ */
int  pci_save_state(struct pci_dev *dev);
void pci_restore_state(struct pci_dev *dev);
int  pci_set_power_state(struct pci_dev *dev, pci_power_t state);
int  pci_enable_wake(struct pci_dev *dev, pci_power_t state, bool enable);

/* ─ drvdata / имя (через встроенную базу) ─ */
static inline void *pci_get_drvdata(struct pci_dev *dev) { return dev_get_drvdata(&dev->dev); }
static inline void  pci_set_drvdata(struct pci_dev *dev, void *data) { dev_set_drvdata(&dev->dev, data); }
static inline const char *pci_name(const struct pci_dev *dev)
{ return dev->lx_name[0] ? dev->lx_name : "0000:00:00.0"; }

/* offline у нас никогда — устройство всегда «на месте». */
static inline int pci_channel_offline(struct pci_dev *pdev) { (void)pdev; return 0; }

/* Внести синтетическое устройство в PCI-ядро Lx_kit (роль перечислителя шины). */
int lx_pci_register_device(struct pci_dev *dev);

#endif /* _LINUX_PCI_H_SHIM */
