/* linux/ethtool.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Для e1000.h нужен лишь тип `struct ethtool_ops` (net_device держит указатель) — полный набор
 * ethtool-операций поднимем, когда возьмёмся за e1000_ethtool.c. Пока — непрозрачный тип. */
#ifndef _LINUX_ETHTOOL_H_SHIM
#define _LINUX_ETHTOOL_H_SHIM

#include <linux/types.h>

struct net_device;

/* EEPROM-дамп через ethtool (e1000_main.c e1000_dump_eeprom). */
struct ethtool_eeprom {
	__u32 cmd;
	__u32 magic;
	__u32 offset;
	__u32 len;
};

/* Полный набор ethtool-операций поднимем с e1000_ethtool.c; пока — то, что дёргает e1000_main.c. */
struct ethtool_ops {
	int (*get_eeprom_len)(struct net_device *dev);
	int (*get_eeprom)(struct net_device *dev, struct ethtool_eeprom *eeprom, u8 *data);
};

/* Скорость/дуплекс линка (uapi/linux/ethtool.h). */
#define SPEED_10      10
#define SPEED_100     100
#define SPEED_1000    1000
#define SPEED_UNKNOWN (-1)
#define DUPLEX_HALF    0x00
#define DUPLEX_FULL    0x01
#define DUPLEX_UNKNOWN 0xff

#endif /* _LINUX_ETHTOOL_H_SHIM */
