/* linux/rtnetlink.h — ШИМ lx_emul (Веха 193), НЕ исходник Linux.
 *
 * В Linux это ЗАМОК конфигурации сети: `rtnl_lock` держит всех, кто меняет интерфейсы, пока идёт
 * одна операция. У нас конфигуратор один — сам драйвер в своём процессе, — и гонки за netdev
 * нет: планировщик Lx_kit кооперативный, второй нити здесь не бывает.
 *
 * Сами заглушки живут в `netdevice.h` (они появились там раньше, вместе с первым драйвером);
 * этот заголовок существует, потому что чужой код включает именно его.
 */
#ifndef _LINUX_RTNETLINK_H_SHIM
#define _LINUX_RTNETLINK_H_SHIM

#include <linux/netdevice.h>

static inline int rtnl_trylock(void) { return 1; }
static inline int rtnl_is_locked(void) { return 1; }

#endif /* _LINUX_RTNETLINK_H_SHIM */
