/* linux/mii.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * Регистры MII/PHY (IEEE 802.3 clause 22) + mii_if_info. e1000 держит и свои PHY-регистры
 * (e1000_hw.h); здесь — общие имена, на которые он ссылается через <linux/mii.h>. */
#ifndef _LINUX_MII_H_SHIM
#define _LINUX_MII_H_SHIM

#include <linux/types.h>

/* Стандартные регистры MII. */
#define MII_BMCR       0x00 /* Basic mode control */
#define MII_BMSR       0x01 /* Basic mode status */
#define MII_PHYSID1    0x02
#define MII_PHYSID2    0x03
#define MII_ADVERTISE  0x04
#define MII_LPA        0x05
#define MII_CTRL1000   0x09
#define MII_STAT1000   0x0a

/* Биты BMCR. */
#define BMCR_RESET     0x8000
#define BMCR_ANENABLE  0x1000
#define BMCR_ANRESTART 0x0200
#define BMCR_FULLDPLX  0x0100
#define BMCR_SPEED100  0x2000

/* Биты BMSR. */
#define BMSR_LSTATUS   0x0004
#define BMSR_ANEGCOMPLETE 0x0020

/* Биты ADVERTISE. */
#define ADVERTISE_CSMA     0x0001
#define ADVERTISE_10HALF   0x0020
#define ADVERTISE_10FULL   0x0040
#define ADVERTISE_100HALF  0x0080
#define ADVERTISE_100FULL  0x0100

struct mii_if_info {
	int phy_id;
	int advertising;
	int phy_id_mask;
	int reg_num_mask;
	struct net_device *dev;
	int (*mdio_read)(struct net_device *dev, int phy_id, int location);
	void (*mdio_write)(struct net_device *dev, int phy_id, int location, int val);
};

/* MII-ioctl'ы (uapi/linux/sockios.h). */
#define SIOCGMIIPHY 0x8947
#define SIOCGMIIREG 0x8948
#define SIOCSMIIREG 0x8949

struct mii_ioctl_data {
	__u16 phy_id;
	__u16 reg_num;
	__u16 val_in;
	__u16 val_out;
};

/* Запрос интерфейса (ndo_eth_ioctl). Полное определение — здесь (netdevice.h делает forward decl). */
struct ifreq {
	char ifr_name[16];
	union {
		void *ifru_data;
		char  ifru_pad[24];
	} ifr_ifru;
};
#define ifr_data ifr_ifru.ifru_data

static inline struct mii_ioctl_data *if_mii(struct ifreq *rq)
{ return (struct mii_ioctl_data *)&rq->ifr_ifru; }

#endif /* _LINUX_MII_H_SHIM */
