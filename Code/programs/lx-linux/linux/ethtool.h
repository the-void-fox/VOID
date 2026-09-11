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
/* Веха 193 — Wake-on-LAN. Пробуждать VOID по сети некому и нечем (сна у нас ещё нет), но
 * драйвер объявляет поддержку в своих ethtool-операциях, и структура обязана существовать. */
#define WAKE_PHY    (1 << 0)
#define WAKE_UCAST  (1 << 1)
#define WAKE_MCAST  (1 << 2)
#define WAKE_BCAST  (1 << 3)
#define WAKE_ARP    (1 << 4)
#define WAKE_MAGIC  (1 << 5)
#define WAKE_MAGICSECURE (1 << 6)

struct ethtool_wolinfo {
	u32 cmd;
	u32 supported;
	u32 wolopts;
	u8  sopass[6];
};

/* ─ Веха 193: поверхность ethtool ─
 *
 * На atl1c её удалось обойти: `atl1c_ethtool.c` просто не линковался, потому что жил отдельным
 * файлом. У `8139too.c` операции ethtool лежат ВНУТРИ самого драйвера, и выбора нет — структуры
 * приходится завести.
 *
 * Утилиты `ethtool` у нас по-прежнему нет, и заполненные драйвером поля никто не читает. Но
 * структура обязана совпадать по ФОРМЕ: `memcpy` в `bus_info` длиной 32 байта не спросит, сколько
 * места мы отвели на самом деле.
 */
struct ethtool_drvinfo {
	u32  cmd;
	char driver[32];
	char version[32];
	char fw_version[32];
	char bus_info[32];
	char erom_version[32];
	char reserved2[12];
	u32  n_priv_flags;
	u32  n_stats;
	u32  testinfo_len;
	u32  eedump_len;
	u32  regdump_len;
};

struct ethtool_regs {
	u32 cmd;
	u32 version;
	u32 len;
	u8  data[0];
};

struct ethtool_stats {
	u32 cmd;
	u32 n_stats;
	u64 data[0];
};

/* Набор строк, который запрашивает ethtool. Нам важен только `ETH_SS_STATS`: по нему драйвер
 * отвечает числом своих счётчиков. */
#define ETH_SS_TEST       0
#define ETH_SS_STATS      1
#define ETH_SS_PRIV_FLAGS 2

/* Современная форма описания линка (пришла на смену `ethtool_cmd`): скорость, дуплекс и битовые
 * карты режимов. Карты у нас сведены к одному слову — драйверам RTL/MII больше и не нужно. */
struct ethtool_link_ksettings {
	struct {
		u32 cmd;
		u32 speed;
		u8  duplex;
		u8  port;
		u8  phy_address;
		u8  autoneg;
		u8  mdio_support;
		u8  eth_tp_mdix;
		u8  eth_tp_mdix_ctrl;
		s8  link_mode_masks_nwords;
	} base;
	struct {
		u32 supported;
		u32 advertising;
		u32 lp_advertising;
	} link_modes;
};

struct ethtool_ops {
	int (*get_eeprom_len)(struct net_device *dev);
	int (*get_eeprom)(struct net_device *dev, struct ethtool_eeprom *eeprom, u8 *data);
	/* Веха 193 — то, что объявляет `8139too`. Порядок полей значения не имеет (назначение по
	 * имени), важно лишь, чтобы имя и тип совпадали с ожиданием чужого кода. */
	void (*get_drvinfo)(struct net_device *dev, struct ethtool_drvinfo *info);
	int  (*get_regs_len)(struct net_device *dev);
	void (*get_regs)(struct net_device *dev, struct ethtool_regs *regs, void *p);
	int  (*nway_reset)(struct net_device *dev);
	u32  (*get_link)(struct net_device *dev);
	u32  (*get_msglevel)(struct net_device *dev);
	void (*set_msglevel)(struct net_device *dev, u32 level);
	void (*get_wol)(struct net_device *dev, struct ethtool_wolinfo *wol);
	int  (*set_wol)(struct net_device *dev, struct ethtool_wolinfo *wol);
	void (*get_strings)(struct net_device *dev, u32 stringset, u8 *data);
	int  (*get_sset_count)(struct net_device *dev, int sset);
	void (*get_ethtool_stats)(struct net_device *dev, struct ethtool_stats *stats, u64 *data);
	int  (*get_link_ksettings)(struct net_device *dev,
				   struct ethtool_link_ksettings *cmd);
	int  (*set_link_ksettings)(struct net_device *dev,
				   const struct ethtool_link_ksettings *cmd);
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

struct mii_if_info;

/* Помощники MII поверх ethtool: драйвер перекладывает на них всю работу с линком. */
int mii_ethtool_get_link_ksettings(struct mii_if_info *mii,
				   struct ethtool_link_ksettings *cmd);
int mii_ethtool_set_link_ksettings(struct mii_if_info *mii,
				   const struct ethtool_link_ksettings *cmd);
int mii_nway_restart(struct mii_if_info *mii);

#endif /* _LINUX_ETHTOOL_H_SHIM */
