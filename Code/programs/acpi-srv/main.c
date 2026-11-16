/* Веха 236 — `acpi-srv`: поднять интерпретатор AML и СПРОСИТЬ прошивку о батарее и крышке.
 *
 * ## Что это такое на этом шаге
 *
 * Не служба ещё, а **проба подъёма**: программа поднимает uACPI по настоящим таблицам машины,
 * находит устройства по их идентификаторам и выполняет те самые методы, ради которых всё
 * затевалось — `_BST`, `_BIF`, `_LID`, `_PSR`. Что получилось и что нет — печатает.
 *
 * Так сделано нарочно. Сервер с протоколом, клиентами и правами поверх непроверенного
 * интерпретатора означал бы, что первая же неудача выглядит как «батарея не показывается», и
 * искать её пришлось бы через три слоя. Сперва пусть заработает ответ, потом — служба.
 *
 * ## Ничего про одну машину
 *
 * Пути вида `\_SB.PCI0.BAT0` здесь не встречаются и не должны: у другого ноутбука они другие.
 * Ищем **по идентификатору** (`PNP0C0A` — батарея, `PNP0C0D` — крышка, `ACPI0003` — адаптер),
 * как это делает всякая операционная система.
 *
 * ## Как представляемся прошивке
 *
 * По [[0026-how-we-answer-firmware|ADR 0026]]: «да» на строки Windows вплоть до той версии,
 * чьё поведение обеспечиваем, «нет» на `_OSI("Linux")`. У uACPI это ровно поведение по
 * умолчанию, поэтому здесь мы только ПЕЧАТАЕМ, кем нас в итоге сочли, — ответ меняет ветку
 * прошивки, и при разборе неполадок на чужой машине это первое, что надо знать.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "syscall.h"

#include <uacpi/event.h>
#include <uacpi/namespace.h>
#include <uacpi/osi.h>
#include <uacpi/status.h>
#include <uacpi/types.h>
#include <uacpi/uacpi.h>
#include <uacpi/utilities.h>

extern void glue_init(uintptr_t hw_cap, uintptr_t ec_cap, uintptr_t fw_cap);
extern int ec_init(void);
extern int ec_install(void);
extern int glue_run_work(void);
extern int tables_build(void);

/* ─── чтение найденных устройств ──────────────────────────────────────────── */

/* `_BST` возвращает пакет из четырёх чисел: состояние, ток, остаток, напряжение. Смысл
 * состояния — биты: 1 разряжается, 2 заряжается, 4 «критически мало». */
static void report_battery(uacpi_namespace_node *node, const char *path) {
    uacpi_object *obj = UACPI_NULL;
    uacpi_status st = uacpi_eval_simple_typed(node, "_BST", UACPI_OBJECT_PACKAGE_BIT, &obj);
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] %s: _BST не ответил (%s)\n", path, uacpi_status_to_string(st));
        return;
    }
    uacpi_object_array arr;
    if (uacpi_object_get_package(obj, &arr) != UACPI_STATUS_OK || arr.count < 4) {
        printf("[acpi] %s: _BST вернул не пакет из четырёх чисел\n", path);
        uacpi_object_unref(obj);
        return;
    }
    uacpi_u64 v[4] = { 0, 0, 0, 0 };
    for (int i = 0; i < 4; i++) {
        uacpi_object_get_integer(arr.objects[i], &v[i]);
    }
    uacpi_object_unref(obj);

    /* 0xFFFFFFFF в ACPI значит «неизвестно». Показать его как число значило бы соврать
     * правдоподобно — говорим словом. */
    const char *state = (v[0] & 4)   ? "критически мало"
                        : (v[0] & 2) ? "заряжается"
                        : (v[0] & 1) ? "разряжается"
                                     : "полна";
    printf("[acpi] %s: %s", path, state);
    if (v[2] == 0xffffffffu) {
        printf(", остаток неизвестен\n");
    } else {
        printf(", остаток %llu мВт·ч, напряжение %llu мВ\n", (unsigned long long)v[2],
               (unsigned long long)v[3]);
    }

    /* Паспорт батареи — отдельным методом; из него берём полную ёмкость, чтобы остаток можно
     * было перевести в проценты. */
    obj = UACPI_NULL;
    st = uacpi_eval_simple_typed(node, "_BIF", UACPI_OBJECT_PACKAGE_BIT, &obj);
    if (st == UACPI_STATUS_OK && uacpi_object_get_package(obj, &arr) == UACPI_STATUS_OK
        && arr.count >= 13) {
        uacpi_u64 full = 0, design = 0;
        uacpi_object_get_integer(arr.objects[1], &design);
        uacpi_object_get_integer(arr.objects[2], &full);
        if (full != 0 && full != 0xffffffffu && v[2] != 0xffffffffu) {
            printf("[acpi] %s: %llu %% (полная %llu, проектная %llu)\n", path,
                   (unsigned long long)(v[2] * 100 / full), (unsigned long long)full,
                   (unsigned long long)design);
        }
    }
    if (obj) {
        uacpi_object_unref(obj);
    }
}

static uacpi_iteration_decision on_battery(void *user, uacpi_namespace_node *node, uacpi_u32 d) {
    const uacpi_char *path = uacpi_namespace_node_generate_absolute_path(node);
    (void)d;
    (*(int *)user)++;
    report_battery(node, path ? path : "?");
    if (path) {
        uacpi_free_absolute_path(path);
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

static uacpi_iteration_decision on_lid(void *user, uacpi_namespace_node *node, uacpi_u32 d) {
    const uacpi_char *path = uacpi_namespace_node_generate_absolute_path(node);
    uacpi_u64 open = 0;
    uacpi_status st = uacpi_eval_simple_integer(node, "_LID", &open);
    (void)d;
    (*(int *)user)++;
    if (st == UACPI_STATUS_OK) {
        printf("[acpi] %s: крышка %s\n", path ? path : "?", open ? "открыта" : "закрыта");
    } else {
        printf("[acpi] %s: _LID не ответил (%s)\n", path ? path : "?",
               uacpi_status_to_string(st));
    }
    if (path) {
        uacpi_free_absolute_path(path);
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

static uacpi_iteration_decision on_ac(void *user, uacpi_namespace_node *node, uacpi_u32 d) {
    const uacpi_char *path = uacpi_namespace_node_generate_absolute_path(node);
    uacpi_u64 online = 0;
    uacpi_status st = uacpi_eval_simple_integer(node, "_PSR", &online);
    (void)d;
    (*(int *)user)++;
    if (st == UACPI_STATUS_OK) {
        printf("[acpi] %s: адаптер %s\n", path ? path : "?", online ? "подключён" : "отключён");
    } else {
        printf("[acpi] %s: _PSR не ответил (%s)\n", path ? path : "?",
               uacpi_status_to_string(st));
    }
    if (path) {
        uacpi_free_absolute_path(path);
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

/* Заглушка под кнопку питания. Пока прерывание SCI ловит ядро, сюда управление не попадёт —
 * см. длинное пояснение в `main`. Когда попадёт, здесь будет чистое выключение сеанса. */
static uacpi_interrupt_ret on_power_button(uacpi_handle ctx) {
    (void)ctx;
    printf("[acpi] кнопка питания (через сервер)\n");
    return UACPI_INTERRUPT_HANDLED;
}

/* ─── подъём ──────────────────────────────────────────────────────────────── */

int main(void) {
    /* Права позиционны (Веха 99): нулевое — `hwprobe`, первое — `ioport:ec`, если выдано.
     * Конфиг: `service acpi-srv hwprobe ioport:ec`. */
    uintptr_t hw = vsys_start_cap(0);
    uintptr_t fw = vsys_start_cap(1);
    uintptr_t ec = vsys_start_cap(2);
    if (hw == VOID_NO_CAP) {
        printf("[acpi] нет права `hwprobe` — без него не видно ни таблиц, ни портов\n");
        return 1;
    }
    glue_init(hw, ec, fw);
    if (ec == VOID_NO_CAP) {
        printf("[acpi] узкого права на порты контроллера нет — хожу общим\n");
    }
    if (fw == VOID_NO_CAP) {
        printf("[acpi] нет права `fwmem` — области памяти прошивки будут недоступны\n");
    }

    if (tables_build() != 0) {
        return 1;
    }

    /* Ранний доступ к таблицам: uACPI разбирает FADT и запоминает регистры ДО того, как
     * появится куча под пространство имён. Буфер её, не наш. */
    static uint8_t early[4096];
    uacpi_status st = uacpi_setup_early_table_access(early, sizeof(early));
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] ранний доступ к таблицам не вышел: %s\n", uacpi_status_to_string(st));
        return 1;
    }

    st = uacpi_initialize(0);
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] инициализация не вышла: %s\n", uacpi_status_to_string(st));
        return 1;
    }

    /* Два разных шага, и путать их нельзя: `load` разбирает байт-код и строит пространство
     * имён, `initialize` ВЫПОЛНЯЕТ `_INI`/`_STA` устройств и зовёт `_REG` — тот самый, без
     * которого встроенный контроллер остаётся недоступен (ADR 0025). */
    st = uacpi_namespace_load();
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] разбор AML не вышел: %s\n", uacpi_status_to_string(st));
        return 1;
    }
    /* Веха 237 — обработчик встроенного контроллера ставится ИМЕННО ЗДЕСЬ, между разбором и
     * подъёмом устройств. Раньше нельзя: пространства имён ещё нет. Позже поздно: `_REG` для
     * областей контроллера выполняется внутри `uacpi_namespace_initialize`, и опоздав, мы
     * получили бы прошивку, уверенную, что контроллера в машине нет. */
    if (ec_init() == 0) {
        ec_install();
    }

    st = uacpi_namespace_initialize();
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] подъём устройств не вышел: %s\n", uacpi_status_to_string(st));
        return 1;
    }
    glue_run_work();

    /* Кем нас сочла прошивка. Печатаем ПОСЛЕ подъёма: спрашивает она в `_INI`, а до того
     * ответа ещё нет. */
    uacpi_vendor_interface iface = uacpi_latest_queried_vendor_interface();
    printf("[acpi] прошивка спросила про Windows (код совместимости %u)\n", (unsigned)iface);

    /* ВЕРНУТЬ КНОПКУ ПИТАНИЯ ЯДРУ (Веха 237.1).
     *
     * uACPI при подъёме ВЫКЛЮЧАЕТ все фиксированные события — и это не её своеволие, а
     * контракт: какие события нужны, решает система, а не библиотека. Беда в том, что у нас
     * кнопку питания уже обслуживает ЯДРО (бит `PWRBTN_EN` в блоке PM1 плюс SCI через IOAPIC,
     * Веха 101), и наш подъём её молча гасил. Снаружи это выглядело ровно так, как рассказал
     * владелец: «система перестала выключаться с одного нажатия».
     *
     * Поэтому включаем бит обратно. Обработчик при этом НЕ ставим: прерывание по-прежнему
     * ловит ядро, и так и должно быть, пока у сервера нет своего SCI. Когда появится — кнопка
     * переедет сюда целиком, вместе с крышкой и кнопкой сна.
     *
     * Урок общий: поднимая чужую подсистему, проверь, что она не выключила ТВОЮ. Молчаливая
     * потеря уже работавшего — худший род поломки, и ловится она только на живой машине.
     *
     * Ставим ОБРАБОТЧИК, а не просто включаем бит: uACPI не даёт включить событие, которое
     * некому обработать, и отвечает «no handler found» — проверено, первая попытка была
     * именно такой. Сам обработчик при этом не сработает никогда: прерывание SCI ловит ядро,
     * а не мы. Он здесь затем, чтобы бит `PWRBTN_EN` остался взведённым для ядра — и затем,
     * чтобы, когда у сервера появится свой SCI, кнопке было куда переехать. */
    st = uacpi_install_fixed_event_handler(UACPI_FIXED_EVENT_POWER_BUTTON, on_power_button, NULL);
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] не вышло вернуть кнопку питания: %s — выключение с кнопки не сработает\n",
               uacpi_status_to_string(st));
    }

    int batteries = 0, lids = 0, acs = 0;
    uacpi_find_devices("PNP0C0A", on_battery, &batteries);
    uacpi_find_devices("PNP0C0D", on_lid, &lids);
    uacpi_find_devices("ACPI0003", on_ac, &acs);
    glue_run_work();

    if (batteries == 0 && lids == 0 && acs == 0) {
        /* Это не поломка: у настольной машины ни батареи, ни крышки и нет. Сказать надо
         * ровно это, а не «не работает». */
        printf("[acpi] батареи, крышки и адаптера эта машина не объявляет\n");
    }
    return 0;
}
