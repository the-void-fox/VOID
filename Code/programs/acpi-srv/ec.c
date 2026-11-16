/* Веха 237 — ДРАЙВЕР ВСТРОЕННОГО КОНТРОЛЛЕРА: обработчик области `EmbeddedControl`.
 *
 * ## Почему это наша работа, а не библиотеки
 *
 * uACPI исполняет байт-код, но железа не касается — за каждым видом области стоит обработчик
 * операционной системы. Для `SystemIO` и `PCI_Config` это две строки (у нас они в `glue.c`), а
 * встроенный контроллер — настоящее устройство со своим протоколом, и драйвер к нему пишет
 * система. В Linux это `drivers/acpi/ec.c`, почти тысяча строк.
 *
 * ## Что он делает
 *
 * У контроллера два порта: командный/статусный и данных (их называет таблица ECDT; на ASUS
 * X54C это 0x66 и 0x62). Разговор — рукопожатие по битам статуса:
 *
 *   чтение:  ждём «вход свободен» → команда 0x80 → ждём → адрес → ждём «выход полон» → байт
 *   запись:  ждём «вход свободен» → команда 0x81 → ждём → адрес → ждём → байт
 *
 * Ждать приходится ПО-НАСТОЯЩЕМУ: контроллер — отдельный микроконтроллер на плате, он отвечает
 * за десятки миллисекунд. Прошивка X54C закладывает на один свой запрос до 127 мс (цикл
 * `Sleep(1)` в методе `RRAM`), и наш потолок не должен быть меньше.
 *
 * ## Чего здесь нет
 *
 * **Очереди запросов `_Qxx`.** Когда контроллеру есть что сообщить (нажали кнопку, сменился
 * заряд), он поднимает бит «событие» и ждёт команды 0x84. Узнать об этом вовремя можно только
 * по прерыванию SCI, которого у нас пока нет, — поэтому опрос событий появится вместе с ним.
 * Чтение и запись работают и без этого: они идут по запросу AML, а не по событию.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "syscall.h"

#include <uacpi/event.h>
#include <uacpi/kernel_api.h>
#include <uacpi/namespace.h>
#include <uacpi/notify.h>
#include <uacpi/opregion.h>
#include <uacpi/status.h>
#include <uacpi/types.h>

extern uintptr_t g_hw_cap;
extern uintptr_t g_ec_cap;

/* Биты статусного порта (ACPI 6.x, §12.2.1). */
#define EC_STS_OBF 0x01 /* выходной буфер полон — есть что прочитать */
#define EC_STS_IBF 0x02 /* входной буфер полон — контроллер ещё не забрал прошлое */

/* Биты статуса (продолжение). */
#define EC_STS_SCI_EVT 0x20 /* контроллеру есть что сообщить */

/* Команды (§12.3). */
#define EC_CMD_READ 0x80
#define EC_CMD_WRITE 0x81
#define EC_CMD_QUERY 0x84

/* Потолок ожидания. Больше, чем закладывает сама прошивка (127 мс), и с запасом на машину, где
 * контроллер вялее. Превышение — не зависание, а отказ с именем: «контроллер не ответил». */
#define EC_TIMEOUT_MS 500

static uint16_t g_cmd_port;
static uint16_t g_data_port;
static int g_ready;

/* Устройство контроллера в пространстве имён (у него живут методы запросов `_Qxx`) и номер
 * его события — оба называет ECDT, но узел находится только после разбора AML. */
static uacpi_namespace_node *g_ec_node;
static uint8_t g_gpe;
static int g_has_gpe;

/* Каким правом ходим. Узкое (`ioport:ec`) — если прошивка объявила ECDT и конфиг его дал;
 * иначе общее. Выбор делается один раз: смешивать их в горячем пути незачем. */
static uintptr_t g_cap;

static int io_in(uint16_t port, uint8_t *out) {
    uintptr_t v = vsys_io_read(g_cap, port, 1);
    if (v == VOID_NO_CAP) {
        return -1;
    }
    *out = (uint8_t)v;
    return 0;
}

static int io_out(uint16_t port, uint8_t val) {
    return vsys_io_write(g_cap, port, 1, val);
}

/* Дождаться, пока биты `mask` статуса примут значение `want`.
 *
 * Уступаем процессор, а не крутимся вхолостую: ожидание тут миллисекундное, и занятый цикл
 * съел бы ядро целиком ради байта. Шаг в миллисекунду — тот же, которым опрашивает сама
 * прошивка. */
static int ec_wait(uint8_t mask, uint8_t want, const char *what) {
    uint64_t deadline = vsys_monotonic_ns() + (uint64_t)EC_TIMEOUT_MS * 1000000ull;
    for (;;) {
        uint8_t sts;
        if (io_in(g_cmd_port, &sts) != 0) {
            printf("[ec] нет права читать статусный порт %#x\n", g_cmd_port);
            return -1;
        }
        if ((sts & mask) == want) {
            return 0;
        }
        if (vsys_monotonic_ns() >= deadline) {
            /* Называем, ЧЕГО ждали: «контроллер не ответил» без этого — тупик, а так сразу
             * видно, принял он команду или не отдал ответ. */
            printf("[ec] контроллер молчит %d мс: %s (статус %#x)\n", EC_TIMEOUT_MS, what, sts);
            return -1;
        }
        vsys_sleep_ns(1000000ull);
    }
}

static int ec_read_byte(uint8_t addr, uint8_t *out) {
    if (ec_wait(EC_STS_IBF, 0, "вход занят перед командой") != 0
        || io_out(g_cmd_port, EC_CMD_READ) != 0
        || ec_wait(EC_STS_IBF, 0, "команда не принята") != 0
        || io_out(g_data_port, addr) != 0
        || ec_wait(EC_STS_OBF, EC_STS_OBF, "ответа нет") != 0) {
        return -1;
    }
    return io_in(g_data_port, out);
}

static int ec_write_byte(uint8_t addr, uint8_t val) {
    if (ec_wait(EC_STS_IBF, 0, "вход занят перед командой") != 0
        || io_out(g_cmd_port, EC_CMD_WRITE) != 0
        || ec_wait(EC_STS_IBF, 0, "команда не принята") != 0
        || io_out(g_data_port, addr) != 0
        || ec_wait(EC_STS_IBF, 0, "адрес не принят") != 0
        || io_out(g_data_port, val) != 0) {
        return -1;
    }
    return ec_wait(EC_STS_IBF, 0, "байт не принят");
}

/* ─── обработчик области ──────────────────────────────────────────────────── */

static uacpi_status ec_region(uacpi_region_op op, uacpi_handle op_data) {
    switch (op) {
    case UACPI_REGION_OP_ATTACH: {
        uacpi_region_attach_data *d = op_data;
        d->out_region_context = NULL;
        return UACPI_STATUS_OK;
    }
    case UACPI_REGION_OP_DETACH:
        return UACPI_STATUS_OK;
    case UACPI_REGION_OP_READ:
    case UACPI_REGION_OP_WRITE: {
        uacpi_region_rw_data *d = op_data;
        /* Адресное пространство контроллера — 256 байт, по одному за обращение. Ширину больше
         * байта разбираем сами: поля в прошивке бывают двух- и четырёхбайтовые (`B0RC`,
         * 16 бит), а протокол другого не знает. Порядок младшим байтом вперёд — как в ACPI. */
        if (d->address > 0xff || d->byte_width == 0 || d->byte_width > 8
            || d->address + d->byte_width > 0x100) {
            return UACPI_STATUS_INVALID_ARGUMENT;
        }
        if (!g_ready) {
            return UACPI_STATUS_NOT_FOUND;
        }
        if (op == UACPI_REGION_OP_READ) {
            uacpi_u64 v = 0;
            for (unsigned i = 0; i < d->byte_width; i++) {
                uint8_t b;
                if (ec_read_byte((uint8_t)(d->address + i), &b) != 0) {
                    return UACPI_STATUS_HARDWARE_TIMEOUT;
                }
                v |= (uacpi_u64)b << (8 * i);
            }
            d->value = v;
        } else {
            for (unsigned i = 0; i < d->byte_width; i++) {
                if (ec_write_byte((uint8_t)(d->address + i),
                                  (uint8_t)(d->value >> (8 * i))) != 0) {
                    return UACPI_STATUS_HARDWARE_TIMEOUT;
                }
            }
        }
        return UACPI_STATUS_OK;
    }
    default:
        return UACPI_STATUS_INVALID_ARGUMENT;
    }
}

/* ─── подъём ──────────────────────────────────────────────────────────────── */

/* Порты берём из ECDT — единственного места, где они названы БЕЗ AML. Это и разрывает
 * курицу с яйцом: чтобы исполнить байт-код, нужен контроллер, а описан он в байт-коде.
 *
 * Раскладка (ACPI 6.x, §5.2.15): со смещения 36 адрес команды, с 48 — данных; в каждом
 * восьмибайтовое поле по +4, а по +0 — пространство (1 = порты). Те же числа читает и ядро;
 * здесь они читаются заново, потому что серверу нужна таблица целиком, а не пара чисел через
 * ещё один вызов. */
int ec_init(void) {
    /* 65 байт — минимум, в котором есть и оба адреса, и номер события (смещение 64). Буфер
     * больше с запасом: дальше в таблице лежит путь к устройству, его читает `ec_find_node`. */
    uint8_t ecdt[128];
    size_t n = vsys_acpi_table(g_hw_cap, "ECDT", 0, ecdt, sizeof(ecdt));
    if (n < 65) {
        printf("[ec] ECDT нет — встроенного контроллера эта машина не объявляет\n");
        return -1;
    }
    if (ecdt[36] != 1 || ecdt[48] != 1) {
        /* Бывают машины, где контроллер отображён в память, а не на порты. Это другой путь, и
         * делать вид, что адрес — номер порта, значило бы писать в случайное место. */
        printf("[ec] контроллер не на портах (пространство %u/%u) — такого мы пока не умеем\n",
               ecdt[36], ecdt[48]);
        return -1;
    }
    uint64_t cmd, data;
    memcpy(&cmd, ecdt + 40, sizeof(cmd));
    memcpy(&data, ecdt + 52, sizeof(data));
    if (cmd == 0 || data == 0 || cmd > 0xffff || data > 0xffff) {
        printf("[ec] ECDT называет негодные порты (%#llx/%#llx)\n",
               (unsigned long long)cmd, (unsigned long long)data);
        return -1;
    }
    g_cmd_port = (uint16_t)cmd;
    g_data_port = (uint16_t)data;
    g_cap = g_ec_cap != VOID_NO_CAP ? g_ec_cap : g_hw_cap;

    /* Проверяем, что порт вообще читается, ДО того как объявим себя готовыми: отказ права
     * здесь выглядел бы потом как «контроллер молчит», и искать его пришлось бы в железе. */
    uint8_t sts;
    if (io_in(g_cmd_port, &sts) != 0) {
        printf("[ec] нет права читать порт %#x — контроллер недоступен\n", g_cmd_port);
        return -1;
    }
    g_ready = 1;
    /* Номер события и путь к устройству — там же, в ECDT (смещения 64 и 65). Путь нужен,
     * чтобы знать, у КОГО спрашивать `_Qxx`: методы запросов живут в самом устройстве. */
    g_gpe = ecdt[64];
    g_has_gpe = 1;
    printf("[ec] контроллер на портах %#x/%#x, статус %#x%s, событие %#x\n", g_cmd_port,
           g_data_port, sts, g_cap == g_ec_cap ? " (узкое право)" : " (общее право)", g_gpe);
    return 0;
}

/* Найти устройство контроллера в пространстве имён по пути из ECDT. Отдельным шагом от
 * `ec_init`, потому что пространства имён в тот момент ещё нет. */
int ec_find_node(void) {
    uint8_t ecdt[128];
    size_t n = vsys_acpi_table(g_hw_cap, "ECDT", 0, ecdt, sizeof(ecdt));
    if (n < 66) {
        return -1;
    }
    /* Путь лежит строкой с нулём на конце; длина таблицы его ограничивает. */
    ecdt[sizeof(ecdt) - 1] = 0;
    const char *path = (const char *)(ecdt + 65);
    if (uacpi_namespace_node_find(NULL, path, &g_ec_node) != UACPI_STATUS_OK) {
        printf("[ec] устройство %s в пространстве имён не найдено\n", path);
        return -1;
    }
    return 0;
}

/* ─── запросы контроллера ─────────────────────────────────────────────────── */

/* Выполнить `_Q<номер>` — метод прошивки, который знает, что означает это событие.
 *
 * Имя собираем из номера: `_Q0A`, `_Q6C`. Шестнадцатеричные цифры ЗАГЛАВНЫЕ — в пространстве
 * имён ACPI имена различаются регистром, и `_q0a` там просто нет. */
static void run_query(uint8_t q) {
    static const char hex[] = "0123456789ABCDEF";
    char name[5] = { '_', 'Q', hex[q >> 4], hex[q & 0xf], 0 };
    uacpi_status st = uacpi_eval(g_ec_node, name, NULL, NULL);
    if (st == UACPI_STATUS_NOT_FOUND) {
        /* Прошивка подняла событие, которого сама не описала. Это её дело, не наше, и падать
         * тут не из-за чего — но сказать стоит: на чужой машине это единственный след. */
        printf("[ec] запрос %s прошивкой не описан\n", name);
        return;
    }
    if (st != UACPI_STATUS_OK) {
        printf("[ec] запрос %s не выполнился: %s\n", name, uacpi_status_to_string(st));
    }
}

/* Обработчик GPE контроллера: спросить, что случилось, и выполнить описанное прошивкой. */
static uacpi_interrupt_ret ec_gpe(uacpi_handle ctx, uacpi_namespace_node *dev, uacpi_u16 idx) {
    (void)ctx;
    (void)dev;
    (void)idx;
    /* Цикл с потолком: один SCI несёт сколько угодно запросов, но если контроллер заклинит и
     * начнёт отвечать бесконечно, бесконечным станет и обработчик. Шестнадцати хватает с
     * запасом — больше за раз не копится. */
    for (int i = 0; i < 16; i++) {
        uint8_t sts;
        if (io_in(g_cmd_port, &sts) != 0 || !(sts & EC_STS_SCI_EVT)) {
            break;
        }
        if (ec_wait(EC_STS_IBF, 0, "вход занят перед запросом") != 0
            || io_out(g_cmd_port, EC_CMD_QUERY) != 0
            || ec_wait(EC_STS_OBF, EC_STS_OBF, "номер запроса не пришёл") != 0) {
            break;
        }
        uint8_t q;
        if (io_in(g_data_port, &q) != 0 || q == 0) {
            break; /* ноль значит «больше нечего» */
        }
        run_query(q);
    }
    return UACPI_INTERRUPT_HANDLED | UACPI_GPE_REENABLE;
}

/* ─── уведомления ─────────────────────────────────────────────────────────── */

/* Что прошивка сообщает про устройство. Коды — из спецификации: 0x80 у батареи значит
 * «состояние изменилось», у крышки — «её тронули», 0x81 у батареи — «паспорт изменился». */
static uacpi_status on_notify(uacpi_handle ctx, uacpi_namespace_node *node, uacpi_u64 value) {
    (void)ctx;
    const uacpi_char *path = uacpi_namespace_node_generate_absolute_path(node);
    printf("[acpi] прошивка сообщает: %s — событие %#llx\n", path ? path : "?",
           (unsigned long long)value);
    if (path) {
        uacpi_free_absolute_path(path);
    }
    /* Пока только называем вслух. Что ДЕЛАТЬ по закрытой крышке (гасить экран, усыплять) —
     * решение не драйвера: его принимает оболочка, и принимать его надо отдельно, вместе с
     * тем, как этот сигнал до неё доедет. */
    return UACPI_STATUS_OK;
}

/* Подключить события контроллера: его GPE (номер даёт ECDT) и приём уведомлений от всех
 * устройств сразу (корень пространства имён). Зовётся ПОСЛЕ подъёма устройств: до него нет
 * ни узлов, на которые вешать, ни работающих `_REG`. */
int ec_events(void) {
    if (!g_has_gpe || !g_ec_node) {
        return -1;
    }
    uacpi_status st = uacpi_install_gpe_handler(NULL, g_gpe, UACPI_GPE_TRIGGERING_EDGE,
                                                ec_gpe, NULL);
    if (st != UACPI_STATUS_OK) {
        printf("[ec] обработчик события %#x не встал: %s\n", g_gpe, uacpi_status_to_string(st));
        return -1;
    }
    st = uacpi_enable_gpe(NULL, g_gpe);
    if (st != UACPI_STATUS_OK) {
        printf("[ec] событие %#x не включилось: %s\n", g_gpe, uacpi_status_to_string(st));
        return -1;
    }
    if (uacpi_install_notify_handler(uacpi_namespace_root(), on_notify, NULL)
        != UACPI_STATUS_OK) {
        printf("[acpi] приём уведомлений не встал — о смене заряда и крышки не узнаем\n");
    }
    printf("[ec] события контроллера включены (GPE %#x)\n", g_gpe);
    return 0;
}

/* Поставить обработчик НА КОРЕНЬ пространства имён, а не на устройство контроллера.
 *
 * Так он достаётся всем областям `EmbeddedControl` машины разом — а их бывает несколько в
 * разных местах дерева (у X54C четыре: `ECOR`, `SMBX`, `SMB2`, `NSBS`). Поставить на
 * устройство значило бы найти его сперва, а найти его можно только исполнив AML, который без
 * обработчика и не исполняется.
 *
 * Зовётся МЕЖДУ разбором и подъёмом устройств: `_REG` для этих областей выполняется в
 * `uacpi_namespace_initialize`, и опоздай мы — прошивка решила бы, что контроллера нет
 * (ровно та ловушка `ECAV`/`ECFL` из ADR 0025). */
int ec_install(void) {
    uacpi_status st = uacpi_install_address_space_handler(
        uacpi_namespace_root(), UACPI_ADDRESS_SPACE_EMBEDDED_CONTROLLER, ec_region, NULL);
    if (st != UACPI_STATUS_OK) {
        printf("[ec] обработчик области не встал: %s\n", uacpi_status_to_string(st));
        return -1;
    }
    return 0;
}
