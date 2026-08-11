/* SPDX-License-Identifier: GPL-2.0 */
/* Шим linux/crc32.h (Веха 131) — CRC-32, каким его зовут сетевые драйверы.
 *
 * Драйверам карт он нужен ровно для одного: хэш многоадресных MAC-адресов, по которому
 * железо решает, поднимать ли кадр наверх. `atl1c_hash_mc_addr` зовёт `ether_crc_le`.
 *
 * Настоящий `lib/crc32.c` мы НЕ портируем, хотя правило фазы — брать код Linux неизменённым.
 * Причина в устройстве самого файла: таблицы там генерируются на этапе сборки ядра
 * (`gen_crc32table`), а под ними лежит слой арх-оптимизаций с выбором реализации в рантайме.
 * Тянуть это значило бы портировать кусок сборочной системы Linux ради функции, которая
 * определена одним полиномом и проверяется одним известным вектором. Тело — в lx_kit.c.
 */
#ifndef _LINUX_CRC32_H
#define _LINUX_CRC32_H

#include <linux/types.h>

/* CRC-32 (IEEE 802.3), младшим битом вперёд — порядок, в котором Ethernet шлёт байты. */
u32 crc32_le(u32 crc, const void *p, size_t len);

static inline u32 crc32(u32 crc, const void *p, size_t len)
{
	return crc32_le(crc, p, len);
}

/* Помощник генерации хэш-таблиц сетевых карт — как в include/linux/crc32.h. */
#define ether_crc_le(length, data) crc32_le(~0, data, length)

#endif /* _LINUX_CRC32_H */
