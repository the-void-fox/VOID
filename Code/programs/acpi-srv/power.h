/* Веха 240 — ПРОТОКОЛ состояния питания: одно место, где записан формат.
 *
 * Зеркало на стороне клиентов — `Code/programs/user/src/acpi_cli.rs`. Два файла, потому что
 * стороны на разных языках; один формат, потому что иначе они разъедутся молча — ровно так
 * уже расходились номера операций у звука (см. `snd_cli.rs`).
 */
#pragma once
#include <stdint.h>

/* Операция: «состояние питания» — `[]` → [`POWER_MSG_LEN`] байт снимка. Других пока нет:
 * сервер отвечает о том, что знает, и ничем не управляет. */
#define OP_POWER 1

/* Веха 241 — СОН: «приготовиться ко сну S3» (`_PTS`) и «мы проснулись» (`_WAK`).
 *
 * Разделено на две операции, потому что между ними машина спит: сам сон делает ЯДРО (сохранить
 * состояние процессора, записать SLP_TYP, пережить потерю всего), а прошивку о нём надо
 * предупредить и потом сказать ей, что всё кончилось, — и это AML, то есть наша работа.
 *
 * Ответ у обеих — один байт: 0 вышло, иначе нет. */
#define OP_SLEEP_PREP 2
#define OP_SLEEP_DONE 3

/* Длина ответа. Фиксированная: поля простые, а разбор переменной длины на двух языках — это
 * два разных разбора. */
#define POWER_MSG_LEN 16

#define POWER_HAS_BATTERY (1u << 0)
#define POWER_HAS_PERCENT (1u << 1)
#define POWER_CHARGING (1u << 2)
#define POWER_DISCHARGING (1u << 3)
#define POWER_CRITICAL (1u << 4)
#define POWER_HAS_AC (1u << 5)
#define POWER_ON_AC (1u << 6)
#define POWER_HAS_LID (1u << 7)
#define POWER_LID_OPEN (1u << 8)

/* Раскладка ответа, байт за байтом:
 *
 *   0..2  флаги (LE, см. `POWER_*`)
 *   2     проценты (значимы при `POWER_HAS_PERCENT`)
 *   3     резерв (ноль)
 *   4..8  остаток, мВт·ч
 *   8..12 полная ёмкость, мВт·ч
 *  12..16 напряжение, мВ
 */

struct power_state {
    /* Флаги `POWER_*`. Шире байта: `POWER_LID_OPEN` — девятый. */
    uint16_t flags;
    uint8_t percent;
    uint32_t remaining;
    uint32_t full;
    uint32_t design;
    uint32_t voltage;
};

void power_find(void);
int power_have(void);
void power_stale(void);
const struct power_state *power_now(void);
int power_pack(uint8_t *out);
void power_report(void);
int power_sleep_prepare(void);
int power_sleep_done(void);
