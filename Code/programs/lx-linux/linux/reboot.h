/* linux/reboot.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux. e1000 трогает только SYS_DOWN в shutdown. */
#ifndef _LINUX_REBOOT_H_SHIM
#define _LINUX_REBOOT_H_SHIM
#define SYS_DOWN    0x0001
#define SYS_RESTART SYS_DOWN
#define SYS_HALT    0x0002
#define SYS_POWER_OFF 0x0003
#endif /* _LINUX_REBOOT_H_SHIM */
