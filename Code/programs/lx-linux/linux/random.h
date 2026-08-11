/* SPDX-License-Identifier: GPL-2.0 */
/* Шим linux/random.h (Веха 131) — случайные байты для драйверов.
 *
 * Единственный потребитель сегодня — генерация MAC-адреса картой, у которой его нет в EEPROM
 * (`eth_random_addr`). Тело — в lx_kit.c: там оно сведено с `SYS_RANDOM` ядра VOID.
 */
#ifndef _LINUX_RANDOM_H_SHIM
#define _LINUX_RANDOM_H_SHIM

#include <linux/types.h>

void get_random_bytes(void *buf, size_t len);

#endif /* _LINUX_RANDOM_H_SHIM */
