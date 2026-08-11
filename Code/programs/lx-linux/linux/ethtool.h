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

/* Веха 131 — режимы линка старым «плоским» видом (`ADVERTISED_*`). Драйвер atl1c держит в них
 * желание пользователя («какие скорости предлагать») и переводит их в биты регистров PHY.
 *
 * Значения — позиции битов из перечисления `ethtool_link_mode_bit_indices` (uapi/linux/ethtool.h,
 * 10baseT_Half = 0 … Autoneg = 6). В самом Linux это делает макрос
 * `__ETHTOOL_LINK_MODE_LEGACY_MASK`; здесь раскрыто, потому что тянуть перечисление на полторы
 * сотни режимов ради семи из них незачем. Важно, что позиции ТЕ ЖЕ: эти числа уходят в конфиг и
 * сравниваются с тем, что показывает Linux на той же карте. */
#define ADVERTISED_10baseT_Half   (1UL << 0)
#define ADVERTISED_10baseT_Full   (1UL << 1)
#define ADVERTISED_100baseT_Half  (1UL << 2)
#define ADVERTISED_100baseT_Full  (1UL << 3)
#define ADVERTISED_1000baseT_Half (1UL << 4)
#define ADVERTISED_1000baseT_Full (1UL << 5)
#define ADVERTISED_Autoneg        (1UL << 6)

#endif /* _LINUX_ETHTOOL_H_SHIM */
