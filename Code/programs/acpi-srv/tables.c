/* Веха 236 — отдать uACPI таблицы прошивки, не имея права на физическую память.
 *
 * ## Задача и почему она неочевидна
 *
 * uACPI начинает с того, что просит физический адрес RSDP, а дальше ходит по памяти сама:
 * RSDP → XSDT → каждая таблица. То есть ей нужна не копия, а КАРТА — и право отображать
 * физические страницы, которого у нас ещё нет (см. `uacpi_kernel_map` в glue.c).
 *
 * Выход тот же, которым пользуется испытательный стенд самой uACPI: собрать **свой** RSDP и
 * свой XSDT в обычной памяти процесса, а таблицы взять копиями у ядра (`SYS_ACPI_TABLE`).
 * Библиотека не отличает — для неё это адреса, которые она попросит отобразить, а наш
 * `uacpi_kernel_map` узнаёт их как свои и возвращает как есть.
 *
 * ## Что при этом ЧЕСТНО, а что — леса
 *
 * Честно: сами таблицы настоящие, байт в байт с машины, вместе с DSDT и всеми SSDT. Разбор
 * AML, пространство имён, методы — всё работает по настоящей прошивке.
 *
 * Леса: адреса ВНУТРИ таблиц (окна `SystemMemory`, FACS, регистры из FADT) по-прежнему
 * физические и по-прежнему недоступны. Поэтому это шаг, а не решение: когда появится узкое
 * право на страницы, объявленные прошивкой, этот файл сократится до «отдать настоящий адрес
 * RSDP».
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "syscall.h"

#include <uacpi/kernel_api.h>
#include <uacpi/status.h>

extern uintptr_t g_hw_cap;

/* ─── что считается «нашей» памятью ───────────────────────────────────────────
 *
 * `uacpi_kernel_map` обязан отличить адрес, который мы сами же и выдумали, от настоящего
 * физического. Храним список выданных кусков: их десяток, список дешевле и честнее догадки
 * «похоже на адрес кучи».
 */
#define BLOCKS_MAX 32
static struct {
    uint64_t addr;
    uint64_t len;
} g_blocks[BLOCKS_MAX];
static int g_blocks_len;

static void remember(const void *p, uint64_t len) {
    if (g_blocks_len < BLOCKS_MAX) {
        g_blocks[g_blocks_len].addr = (uint64_t)(uintptr_t)p;
        g_blocks[g_blocks_len].len = len;
        g_blocks_len++;
    }
}

int glue_own_memory(uint64_t addr, uint64_t len) {
    for (int i = 0; i < g_blocks_len; i++) {
        /* Проверяем ВЕСЬ запрошенный отрезок, а не только начало: запрос, вылезающий за край
         * нашей копии, отдал бы чужую память под видом таблицы. */
        if (addr >= g_blocks[i].addr && addr + len <= g_blocks[i].addr + g_blocks[i].len) {
            return 1;
        }
    }
    return 0;
}

/* ─── заголовок таблицы ACPI ──────────────────────────────────────────────── */

struct sdt_hdr {
    char signature[4];
    uint32_t length;
    uint8_t revision;
    uint8_t checksum;
    char oem_id[6];
    char oem_table_id[8];
    uint32_t oem_revision;
    char creator_id[4];
    uint32_t creator_revision;
};

static void fix_checksum(void *table, size_t len) {
    uint8_t *b = table;
    uint8_t sum = 0;
    b[9] = 0; /* поле контрольной суммы — девятый байт заголовка */
    for (size_t i = 0; i < len; i++) {
        sum = (uint8_t)(sum + b[i]);
    }
    b[9] = (uint8_t)(0u - sum);
}

/* Снять таблицу у ядра. Размер спрашиваем в два приёма: сперва малым буфером читаем
 * заголовок (в нём длина), потом берём целиком. Иначе пришлось бы держать потолок «на всякий
 * случай», а DSDT у разных машин отличается в тридцать раз. */
static void *fetch(const char sig[4], unsigned idx, size_t *out_len) {
    uint8_t head[36];
    size_t n = vsys_acpi_table(g_hw_cap, sig, idx, head, sizeof(head));
    if (n < sizeof(head)) {
        return NULL;
    }
    uint32_t len;
    memcpy(&len, head + 4, sizeof(len));
    if (len < sizeof(head) || len > 4u * 1024 * 1024) {
        printf("[acpi] таблица %.4s №%u: длина %u не похожа на правду — пропуск\n",
               sig, idx, (unsigned)len);
        return NULL;
    }
    void *buf = malloc(len);
    if (!buf) {
        return NULL;
    }
    n = vsys_acpi_table(g_hw_cap, sig, idx, buf, len);
    if (n != len) {
        /* Молча принятый обрезок выглядел бы как успех, а разбор AML выдал бы правдоподобный
         * мусор — ровно та ошибка, которую мы ловили приёмником таблиц на хосте. */
        printf("[acpi] таблица %.4s №%u пришла обрезанной: %u из %u байт\n", sig, idx,
               (unsigned)n, (unsigned)len);
        free(buf);
        return NULL;
    }
    remember(buf, len);
    *out_len = len;
    return buf;
}

/* ─── сборка корня ────────────────────────────────────────────────────────── */

static uint8_t *g_rsdp;

/* Сколько таблиц кладём в свой XSDT. DSDT в него не входит — её называет FADT. */
#define XSDT_MAX 24

int tables_build(void) {
    size_t len;
    uint64_t entries[XSDT_MAX];
    int n_entries = 0;

    /* FADT обязательна: без неё нет ни DSDT, ни регистров управления питанием. */
    void *fadt = fetch("FACP", 0, &len);
    if (!fadt) {
        printf("[acpi] FADT не отдана ядром — поднимать нечего\n");
        return -1;
    }
    size_t fadt_len = len;

    size_t dsdt_len;
    void *dsdt = fetch("DSDT", 0, &dsdt_len);
    if (!dsdt) {
        printf("[acpi] DSDT не отдана ядром — исполнять нечего\n");
        return -1;
    }

    /* Указываем FADT на НАШУ копию DSDT. Поле `X_DSDT` (смещение 140) восьмибайтовое — туда
     * адрес и влезает; старое 32-битное `DSDT` (смещение 40) обнуляем, иначе uACPI имела бы
     * два разных ответа на один вопрос и по спецификации должна предпочесть старое. */
    if (fadt_len >= 148) {
        uint64_t p = (uint64_t)(uintptr_t)dsdt;
        memcpy((uint8_t *)fadt + 140, &p, sizeof(p));
        uint32_t zero = 0;
        memcpy((uint8_t *)fadt + 40, &zero, sizeof(zero));
    } else if (fadt_len >= 44) {
        uint32_t p32 = (uint32_t)(uintptr_t)dsdt;
        if ((uintptr_t)dsdt >> 32) {
            printf("[acpi] старая FADT (без X_DSDT), а копия DSDT выше четырёх гигабайт\n");
            return -1;
        }
        memcpy((uint8_t *)fadt + 40, &p32, sizeof(p32));
    }
    fix_checksum(fadt, fadt_len);
    entries[n_entries++] = (uint64_t)(uintptr_t)fadt;

    /* Все SSDT подряд, пока ядро их отдаёт: батарея вполне может быть описана во второй. */
    for (unsigned i = 0; i < 8 && n_entries < XSDT_MAX; i++) {
        void *t = fetch("SSDT", i, &len);
        if (!t) {
            break;
        }
        entries[n_entries++] = (uint64_t)(uintptr_t)t;
    }

    /* Остальные, которые нам пригодятся: порты контроллера, прерывания, окно конфигурации
     * PCI, таймер. Необязательные — чего нет, того нет. */
    static const char *const extra[] = { "ECDT", "APIC", "MCFG", "HPET" };
    for (unsigned i = 0; i < sizeof(extra) / sizeof(extra[0]) && n_entries < XSDT_MAX; i++) {
        void *t = fetch(extra[i], 0, &len);
        if (t) {
            entries[n_entries++] = (uint64_t)(uintptr_t)t;
        }
    }

    /* XSDT: заголовок плюс восьмибайтовые адреса. */
    size_t xsdt_len = sizeof(struct sdt_hdr) + (size_t)n_entries * 8;
    uint8_t *xsdt = calloc(1, xsdt_len);
    if (!xsdt) {
        return -1;
    }
    struct sdt_hdr *xh = (struct sdt_hdr *)xsdt;
    memcpy(xh->signature, "XSDT", 4);
    xh->length = (uint32_t)xsdt_len;
    xh->revision = 1;
    memcpy(xh->oem_id, "VOID  ", 6);
    memcpy(xh->oem_table_id, "VOIDACPI", 8);
    xh->oem_revision = 1;
    memcpy(xh->creator_id, "VOID", 4);
    xh->creator_revision = 1;
    memcpy(xsdt + sizeof(struct sdt_hdr), entries, (size_t)n_entries * 8);
    fix_checksum(xsdt, xsdt_len);
    remember(xsdt, xsdt_len);

    /* RSDP второй версии: у неё две контрольные суммы — старая по первым двадцати байтам и
     * расширенная по всей структуре. Обе обязаны сойтись, иначе uACPI таблицу не примет. */
    g_rsdp = calloc(1, 36);
    if (!g_rsdp) {
        return -1;
    }
    memcpy(g_rsdp, "RSD PTR ", 8);
    memcpy(g_rsdp + 9, "VOID  ", 6);
    g_rsdp[15] = 2; /* revision 2 = есть XSDT */
    uint32_t rsdt_zero = 0;
    memcpy(g_rsdp + 16, &rsdt_zero, 4);
    uint32_t rsdp_len = 36;
    memcpy(g_rsdp + 20, &rsdp_len, 4);
    uint64_t xsdt_addr = (uint64_t)(uintptr_t)xsdt;
    memcpy(g_rsdp + 24, &xsdt_addr, 8);
    uint8_t sum = 0;
    for (int i = 0; i < 20; i++) {
        sum = (uint8_t)(sum + g_rsdp[i]);
    }
    g_rsdp[8] = (uint8_t)(0u - sum);
    sum = 0;
    for (int i = 0; i < 36; i++) {
        sum = (uint8_t)(sum + g_rsdp[i]);
    }
    g_rsdp[32] = (uint8_t)(0u - sum);
    remember(g_rsdp, 36);

    printf("[acpi] таблицы собраны: FADT, DSDT %u КиБ, всего в XSDT %d\n",
           (unsigned)(dsdt_len / 1024), n_entries);
    return 0;
}

uacpi_status uacpi_kernel_get_rsdp(uacpi_phys_addr *out) {
    if (!g_rsdp) {
        return UACPI_STATUS_NOT_FOUND;
    }
    *out = (uacpi_phys_addr)(uintptr_t)g_rsdp;
    return UACPI_STATUS_OK;
}
