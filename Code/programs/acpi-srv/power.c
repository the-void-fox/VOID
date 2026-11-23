/* Веха 240 — СОСТОЯНИЕ ПИТАНИЯ: то, ради чего заводился интерпретатор AML.
 *
 * ## Зачем отдельный файл
 *
 * До этой вехи сервер называл заряд вслух один раз на подъёме и жил дальше молча. Показать его
 * человеку было нечем: панель про батарею не знает ничего, а узнать может только спросив того,
 * кто исполняет AML. Здесь — та сторона разговора, которая ЗНАЕТ: поиск устройств один раз,
 * снимок состояния и его обновление.
 *
 * ## Почему снимок, а не «спросить прошивку на каждый вопрос»
 *
 * `_BST` — это исполнение байт-кода, которое лезет в встроенный контроллер по портам и берёт
 * при этом общий с прошивкой замок. Панель перерисовывается раз в секунду, окон с индикатором
 * может быть несколько, и каждый такой вопрос стоил бы разговора с железом. Поэтому: снимок
 * обновляется НЕ ЧАЩЕ раза в [`FRESH_NS`] и сразу же — когда прошивка сама сказала, что
 * состояние изменилось (уведомление 0x80). Заряд меняется минутами, а не миллисекундами.
 *
 * ## Ничего про одну машину
 *
 * Устройства ищутся по идентификаторам (`PNP0C0A` — батарея, `PNP0C0D` — крышка, `ACPI0003` —
 * адаптер), пути вида `\_SB.PCI0.BAT0` здесь не встречаются и встречаться не должны.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "syscall.h"

#include <uacpi/namespace.h>
#include <uacpi/sleep.h>
#include <uacpi/status.h>
#include <uacpi/types.h>
#include <uacpi/uacpi.h>
#include <uacpi/utilities.h>

#include "power.h"

/* Насколько снимок считается свежим. Секунда — это и шаг перерисовки панели, и предел, за
 * которым человек заметил бы запаздывание индикатора. */
#define FRESH_NS 1000000000ull

/* Найденные устройства. По одному каждого вида: машин с двумя батареями мы не видели, а
 * выдумывать под них протокол, не имея, на чём проверить, — обещание вслепую. */
static uacpi_namespace_node *g_bat;
static uacpi_namespace_node *g_lid;
static uacpi_namespace_node *g_ac;

static struct power_state g_now;
/* Когда снят снимок (монотонные наносекунды). 0 — не снимался ни разу. */
static uint64_t g_at;
/* Полная ёмкость из паспорта (`_BIF`): спрашивается отдельно и меняется редко. */
static uint32_t g_full;
static uint32_t g_design;

static uacpi_iteration_decision take_bat(void *u, uacpi_namespace_node *n, uacpi_u32 d) {
    (void)u;
    (void)d;
    if (!g_bat) {
        g_bat = n;
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

static uacpi_iteration_decision take_lid(void *u, uacpi_namespace_node *n, uacpi_u32 d) {
    (void)u;
    (void)d;
    if (!g_lid) {
        g_lid = n;
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

static uacpi_iteration_decision take_ac(void *u, uacpi_namespace_node *n, uacpi_u32 d) {
    (void)u;
    (void)d;
    if (!g_ac) {
        g_ac = n;
    }
    return UACPI_ITERATION_DECISION_CONTINUE;
}

/* Найти устройства. Зовётся один раз, после подъёма пространства имён. */
void power_find(void) {
    uacpi_find_devices("PNP0C0A", take_bat, NULL);
    uacpi_find_devices("PNP0C0D", take_lid, NULL);
    uacpi_find_devices("ACPI0003", take_ac, NULL);
}

int power_have(void) {
    return g_bat || g_lid || g_ac;
}

/* Паспорт батареи: полная ёмкость и проектная. Спрашивается отдельно от состояния — он
 * меняется только при замене батареи (и прошивка об этом сообщает событием 0x81). */
static void read_bif(void) {
    uacpi_object *obj = UACPI_NULL;
    if (uacpi_eval_simple_typed(g_bat, "_BIF", UACPI_OBJECT_PACKAGE_BIT, &obj)
        != UACPI_STATUS_OK) {
        return;
    }
    uacpi_object_array arr;
    if (uacpi_object_get_package(obj, &arr) == UACPI_STATUS_OK && arr.count >= 13) {
        uacpi_u64 full = 0, design = 0;
        uacpi_object_get_integer(arr.objects[1], &design);
        uacpi_object_get_integer(arr.objects[2], &full);
        if (full != 0 && full != 0xffffffffu) {
            g_full = (uint32_t)full;
        }
        if (design != 0 && design != 0xffffffffu) {
            g_design = (uint32_t)design;
        }
    }
    uacpi_object_unref(obj);
}

/* Снять состояние заново: `_BST` у батареи, `_PSR` у адаптера, `_LID` у крышки. */
static void refresh(void) {
    struct power_state s;
    memset(&s, 0, sizeof s);

    if (g_bat) {
        if (!g_full) {
            read_bif();
        }
        uacpi_object *obj = UACPI_NULL;
        if (uacpi_eval_simple_typed(g_bat, "_BST", UACPI_OBJECT_PACKAGE_BIT, &obj)
            == UACPI_STATUS_OK) {
            uacpi_object_array arr;
            if (uacpi_object_get_package(obj, &arr) == UACPI_STATUS_OK && arr.count >= 4) {
                uacpi_u64 v[4] = { 0, 0, 0, 0 };
                for (int i = 0; i < 4; i++) {
                    uacpi_object_get_integer(arr.objects[i], &v[i]);
                }
                s.flags |= POWER_HAS_BATTERY;
                if (v[0] & 2) {
                    s.flags |= POWER_CHARGING;
                }
                if (v[0] & 1) {
                    s.flags |= POWER_DISCHARGING;
                }
                if (v[0] & 4) {
                    s.flags |= POWER_CRITICAL;
                }
                /* 0xFFFFFFFF в ACPI значит «неизвестно». Выдать его за число значило бы
                 * соврать правдоподобно: пусть лучше процента не будет вовсе. */
                if (v[2] != 0xffffffffu) {
                    s.remaining = (uint32_t)v[2];
                    if (g_full) {
                        uint64_t pct = (uint64_t)s.remaining * 100 / g_full;
                        s.percent = (uint8_t)(pct > 100 ? 100 : pct);
                        s.flags |= POWER_HAS_PERCENT;
                    }
                }
                if (v[3] != 0xffffffffu) {
                    s.voltage = (uint32_t)v[3];
                }
            }
            uacpi_object_unref(obj);
        }
        s.full = g_full;
        s.design = g_design;
    }

    if (g_ac) {
        uacpi_u64 online = 0;
        if (uacpi_eval_simple_integer(g_ac, "_PSR", &online) == UACPI_STATUS_OK) {
            s.flags |= POWER_HAS_AC;
            if (online) {
                s.flags |= POWER_ON_AC;
            }
        }
    }

    if (g_lid) {
        uacpi_u64 open = 0;
        if (uacpi_eval_simple_integer(g_lid, "_LID", &open) == UACPI_STATUS_OK) {
            s.flags |= POWER_HAS_LID;
            if (open) {
                s.flags |= POWER_LID_OPEN;
            }
        }
    }

    g_now = s;
    g_at = vsys_monotonic_ns();
}

void power_stale(void) {
    g_at = 0;
}

const struct power_state *power_now(void) {
    uint64_t now = vsys_monotonic_ns();
    if (g_at == 0 || now - g_at >= FRESH_NS) {
        refresh();
    }
    return &g_now;
}

/* Разложить снимок в ответ клиенту. Формат — в [[acpi-cli]] (`power.h` и его зеркало на
 * стороне Rust): по одному месту на обе стороны, иначе они разъедутся молча. */
int power_pack(uint8_t *out) {
    const struct power_state *s = power_now();
    out[0] = (uint8_t)(s->flags & 0xff);
    out[1] = (uint8_t)(s->flags >> 8);
    out[2] = s->percent;
    out[3] = 0;
    memcpy(out + 4, &s->remaining, 4);
    memcpy(out + 8, &s->full, 4);
    memcpy(out + 12, &s->voltage, 4);
    return POWER_MSG_LEN;
}

/* Сказать состояние словами — один раз на подъёме. Это не замена индикатору в панели, а
 * ответ на вопрос «работает ли вообще»: журнал читают, когда не работает. */
void power_report(void) {
    const struct power_state *s = power_now();
    if (!power_have()) {
        printf("[acpi] батареи, крышки и адаптера эта машина не объявляет\n");
        return;
    }
    if (s->flags & POWER_HAS_BATTERY) {
        const char *state = (s->flags & POWER_CRITICAL)      ? "критически мало"
                            : (s->flags & POWER_CHARGING)    ? "заряжается"
                            : (s->flags & POWER_DISCHARGING) ? "разряжается"
                                                             : "полна";
        if (s->flags & POWER_HAS_PERCENT) {
            printf("[acpi] батарея: %u %%, %s (%u из %u мВт·ч, %u мВ)\n", s->percent, state,
                   s->remaining, s->full, s->voltage);
        } else {
            printf("[acpi] батарея: %s, заряд неизвестен\n", state);
        }
    }
    if (s->flags & POWER_HAS_AC) {
        printf("[acpi] адаптер %s\n", (s->flags & POWER_ON_AC) ? "подключён" : "отключён");
    }
    if (s->flags & POWER_HAS_LID) {
        printf("[acpi] крышка %s\n", (s->flags & POWER_LID_OPEN) ? "открыта" : "закрыта");
    }
}

/* ─── сон (Веха 241) ──────────────────────────────────────────────────────── */

/* Приготовиться ко сну S3: выполнить `_PTS` (и `_TTS`, если прошивка их объявляет).
 *
 * Это обязательная часть уговора с прошивкой: ею она гасит то, что гасит сама, и приводит
 * встроенный контроллер в состояние, из которого умеет просыпаться. Пропустить её значит
 * уснуть «почти правильно» — а чем кончается «почти» в разговоре с прошивкой, видно по
 * истории с `_REG` (ADR 0025): молчаливым отказом там, где ждёшь ответа.
 *
 * Сам сон делает ядро: записать SLP_TYP можно только с сохранённым состоянием процессора и
 * запрещёнными прерываниями, а это не наше место. */
int power_sleep_prepare(void) {
    uacpi_status st = uacpi_prepare_for_sleep_state(UACPI_SLEEP_STATE_S3);
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] прошивка не дала приготовиться ко сну: %s\n", uacpi_status_to_string(st));
        return -1;
    }
    return 0;
}

/* Проснулись: выполнить `_WAK` и снять то, что прошивка выставила на пробуждении.
 *
 * `uacpi_wake_from_sleep_state` заодно отбрасывает «нажатия» кнопок питания и сна, которые
 * прошивка выставляет самим фактом пробуждения, — без этого машина выключилась бы сразу после
 * того, как её разбудили. Ядро снимает те же биты со своей стороны; две страховки здесь не
 * лишние, потому что порядок между нами и прошивкой не наш. */
int power_sleep_done(void) {
    uacpi_prepare_for_wake_from_sleep_state(UACPI_SLEEP_STATE_S3);
    uacpi_status st = uacpi_wake_from_sleep_state(UACPI_SLEEP_STATE_S3);
    /* Снимок устарел заведомо: пока спали, могли выдернуть кабель. */
    power_stale();
    if (st != UACPI_STATUS_OK) {
        printf("[acpi] `_WAK` не отработал: %s\n", uacpi_status_to_string(st));
        return -1;
    }
    return 0;
}
