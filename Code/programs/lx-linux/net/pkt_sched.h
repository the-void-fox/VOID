/* net/pkt_sched.h — ШИМ lx_emul (Веха 68), НЕ исходник Linux.
 * e1000 берёт отсюда только константу таймаута watchdog по умолчанию. */
#ifndef _NET_PKT_SCHED_H_SHIM
#define _NET_PKT_SCHED_H_SHIM
#include <linux/jiffies.h>
#define DEFAULT_TX_QUEUE_LEN 1000
#endif /* _NET_PKT_SCHED_H_SHIM */
