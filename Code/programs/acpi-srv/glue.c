/* Веха 236 — ПРОСЛОЙКА между uACPI и VOID: сорок функций, которые чужая библиотека ждёт от
 * операционной системы ([[0025-aml-foreign-interpreter|ADR 0025]]).
 *
 * ## Что это за файл
 *
 * uACPI — интерпретатор AML, а не драйвер: она умеет разбирать байт-код прошивки и исполнять
 * его методы, но ВСЁ, чем она касается машины, обязана дать система. Отсюда `uacpi_kernel_*`:
 * выделить память, прочитать порт, подождать, взять замок. Для нас это граница ответственности
 * — ровно здесь кончается чужое и начинается наше.
 *
 * ## Чего здесь сознательно НЕТ (и почему это лучше, чем заглушка, которая врёт)
 *
 * Три вещи VOID пока не умеет, и все три возвращают отказ, а НЕ правдоподобный ноль:
 *
 * - **отображение физической памяти** (`uacpi_kernel_map`) — у нас право на окно MMIO выдаётся
 *   под конкретное устройство, а области AML вида `SystemMemory` (у X54C их десятки) лежат в
 *   памяти, объявленной прошивкой. Нужен отдельный узкий вид права; до него такие области
 *   недоступны;
 * - **прерывание SCI** — нужен IRQ-cap на вектор из FADT;
 * - **отложенная работа в отдельной нити** — сервер пока однопоточный.
 *
 * Соблазн был вернуть нули: тогда `_BST` «работал» бы и показывал заряд 0xFFFFFFFF, а разбирать
 * это пришлось бы месяц. Отказ громче и дешевле (см. [[void-strictness]]).
 *
 * ## Права
 *
 * Берутся ПОЗИЦИОННО из стартовых, как во всей системе (порядок прав значим, Веха 99):
 *   0 — `hwprobe` (таблицы прошивки, порты, конфигурация PCI),
 *   1 — `ioport:ec` (узкое право на порты встроенного контроллера; может отсутствовать).
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "syscall.h"

#include <uacpi/kernel_api.h>
#include <uacpi/status.h>

/* ─── права и общее состояние ─────────────────────────────────────────────── */

uintptr_t g_hw_cap = VOID_NO_CAP;  /* hwprobe: таблицы, порты, PCI */
uintptr_t g_ec_cap = VOID_NO_CAP;  /* ioport:ec — узкое право, если выдано */
uintptr_t g_fw_cap = VOID_NO_CAP;  /* fwmem — страницы, объявленные прошивкой */
uintptr_t g_sci_cap = VOID_NO_CAP; /* irq:sci — прерывание прошивки */

/* Обработчик прерывания, который поставила uACPI. Зовёт его наш цикл событий, а не ядро:
 * прерывание ловит ядро и будит нас, а разбор регистров прошивки — уже здесь. */
static uacpi_interrupt_handler g_sci_handler;
static uacpi_handle g_sci_ctx;

static uint64_t g_boot_ns;

void glue_init(uintptr_t hw_cap, uintptr_t ec_cap, uintptr_t fw_cap, uintptr_t sci_cap) {
    g_hw_cap = hw_cap;
    g_ec_cap = ec_cap;
    g_fw_cap = fw_cap;
    g_sci_cap = sci_cap;
    g_boot_ns = vsys_monotonic_ns();
}

/* Про каждую неподдержанную вещь говорим ОДИН раз: прошивка зовёт их в цикле, и сотня
 * одинаковых строк в журнале прячет остальное. */
#define SAY_ONCE(flag, ...)            \
    do {                               \
        static int flag##_said = 0;    \
        if (!flag##_said) {            \
            flag##_said = 1;           \
            printf(__VA_ARGS__);       \
        }                              \
    } while (0)

/* ─── память ──────────────────────────────────────────────────────────────── */

void *uacpi_kernel_alloc(uacpi_size size) { return malloc(size); }

void *uacpi_kernel_alloc_zeroed(uacpi_size size) { return calloc(1, size); }

void uacpi_kernel_free(void *mem) { free(mem); }

/* ─── журнал ──────────────────────────────────────────────────────────────── */

void uacpi_kernel_log(uacpi_log_level lvl, const uacpi_char *text) {
    /* Уровень называем словом, а не цифрой: журнал читает человек с чужой машины, и «3» ему
     * ничего не говорит. Перевода строки не добавляем — uACPI кладёт свой. */
    const char *tag = lvl == UACPI_LOG_ERROR ? "ошибка"
                    : lvl == UACPI_LOG_WARN  ? "внимание"
                    : lvl == UACPI_LOG_INFO  ? "acpi"
                    : lvl == UACPI_LOG_TRACE ? "след"
                                             : "отладка";
    printf("[%s] %s", tag, text);
}

/* ─── часы, сон, пауза ────────────────────────────────────────────────────── */

uacpi_u64 uacpi_kernel_get_nanoseconds_since_boot(void) {
    return vsys_monotonic_ns() - g_boot_ns;
}

void uacpi_kernel_sleep(uacpi_u64 msec) { vsys_sleep_ns(msec * 1000000ull); }

void uacpi_kernel_stall(uacpi_u8 usec) {
    /* `Stall` в ACPI — ЗАНЯТОЕ ожидание: его зовут там, где уступать процессор нельзя (до
     * 100 мкс по спецификации). Наш `SYS_SLEEP` снял бы задачу с процессора и вернул её
     * миллисекундами позже — для рукопожатия с контроллером это слишком грубо. Поэтому
     * крутимся по монотонным часам. */
    uint64_t until = vsys_monotonic_ns() + (uint64_t)usec * 1000ull;
    while (vsys_monotonic_ns() < until) {
    }
}

/* ─── нити, замки, семафоры ───────────────────────────────────────────────────
 *
 * Сервер однопоточный, и это не временная лень, а решение: интерпретатор AML по
 * спецификации исполняется строго последовательно, а мьютексы AML (`Acquire`/`Release`)
 * разруливает сама uACPI поверх этих примитивов. Одна нить означает, что замок не может
 * быть занят кем-то другим, — значит честная реализация здесь и есть пустая.
 *
 * Если сервер однажды станет многопоточным, эти функции придётся написать по-настоящему, и
 * место названо вслух, чтобы это не выяснилось отладкой.
 */

uacpi_thread_id uacpi_kernel_get_thread_id(void) { return (uacpi_thread_id)1; }

uacpi_handle uacpi_kernel_create_spinlock(void) { return (uacpi_handle)1; }
void uacpi_kernel_free_spinlock(uacpi_handle h) { (void)h; }
uacpi_cpu_flags uacpi_kernel_lock_spinlock(uacpi_handle h) { (void)h; return 0; }
void uacpi_kernel_unlock_spinlock(uacpi_handle h, uacpi_cpu_flags f) { (void)h; (void)f; }

uacpi_interrupt_state uacpi_kernel_disable_interrupts(void) { return 0; }
void uacpi_kernel_restore_interrupts(uacpi_interrupt_state s) { (void)s; }

/* Семафор при одной нити — просто счётчик: ждать некого, разбудить некому. Но счёт ведём
 * настоящий: uACPI проверяет им собственные инварианты, и соврать «всегда можно» значило бы
 * спрятать её же ошибку. */
struct sem {
    uint32_t units;
};

uacpi_handle uacpi_kernel_create_semaphore(uacpi_u32 initial_units) {
    struct sem *s = malloc(sizeof(*s));
    if (s) {
        s->units = initial_units;
    }
    return (uacpi_handle)s;
}

void uacpi_kernel_free_semaphore(uacpi_handle h) { free(h); }

uacpi_status uacpi_kernel_wait_for_semaphore(uacpi_handle h, uacpi_u16 timeout) {
    struct sem *s = h;
    (void)timeout;
    if (s->units == 0) {
        /* Одна нить и нулевой счёт — это не «подожди», это тупик: ждать нечего, добавить
         * единицу некому. Говорим вслух, а не висим до таймаута. */
        return UACPI_STATUS_TIMEOUT;
    }
    s->units--;
    return UACPI_STATUS_OK;
}

void uacpi_kernel_signal_semaphore(uacpi_handle h) {
    struct sem *s = h;
    s->units++;
}

/* ─── отложенная работа ───────────────────────────────────────────────────────
 *
 * uACPI откладывает сюда обработку уведомлений и запросов встроенного контроллера. Исполнять
 * прямо в `schedule_work` нельзя: её зовут изнутри интерпретатора, из-под его замков, и
 * обработчик имеет право вызвать AML снова. Поэтому — очередь, которую разбирает главный цикл.
 */
struct work {
    uacpi_work_handler handler;
    uacpi_handle ctx;
    int queued;
};

#define WORK_MAX 32
static struct work *g_queue[WORK_MAX];
static int g_queue_len;

uacpi_handle uacpi_kernel_create_work_item(void) { return calloc(1, sizeof(struct work)); }

void uacpi_kernel_free_work_item(uacpi_handle h) { free(h); }

void uacpi_kernel_schedule_work(uacpi_work_type type, uacpi_handle item,
                                uacpi_work_handler handler, uacpi_handle ctx) {
    struct work *w = item;
    (void)type;
    w->handler = handler;
    w->ctx = ctx;
    if (g_queue_len >= WORK_MAX) {
        /* Молча ронять работу нельзя: это потерянное событие батареи или крышки, и выглядит
         * оно как «система иногда не замечает». Говорим и считаем. */
        printf("[acpi] очередь отложенной работы переполнена — событие потеряно\n");
        return;
    }
    w->queued = 1;
    g_queue[g_queue_len++] = w;
}

/* Разобрать очередь. Зовётся главным циклом и из `wait_for_work_completion`. */
int glue_run_work(void) {
    int done = 0;
    while (g_queue_len > 0) {
        /* Берём с начала и СДВИГАЕМ: обработчик вправе поставить работу снова, и порядок
         * событий прошивки терять нельзя. */
        struct work *w = g_queue[0];
        for (int i = 1; i < g_queue_len; i++) {
            g_queue[i - 1] = g_queue[i];
        }
        g_queue_len--;
        /* Состояние элемента обновляем ДО вызова: uACPI разрешает обработчику переиспользовать
         * и даже освободить его, так что после вызова трогать нечего. */
        uacpi_work_handler handler = w->handler;
        uacpi_handle ctx = w->ctx;
        w->queued = 0;
        handler(ctx);
        done++;
    }
    return done;
}

uacpi_status uacpi_kernel_wait_for_work_completion(void) {
    glue_run_work();
    return UACPI_STATUS_OK;
}

/* ─── порты ввода-вывода ──────────────────────────────────────────────────────
 *
 * Диапазон «отображать» нечего: порты адресуются напрямую. Ручка хранит базу и длину, чтобы
 * смещения проверялись у нас, а не превращались в обращение к чужому порту.
 */
struct io_range {
    uint16_t base;
    uint16_t len;
    uintptr_t cap; /* чем ходить: узкое право на контроллер либо общий hwprobe */
};

uacpi_status uacpi_kernel_io_map(uacpi_io_addr base, uacpi_size len, uacpi_handle *out) {
    struct io_range *r;
    if (base > 0xffff || len == 0 || base + len > 0x10000) {
        return UACPI_STATUS_INVALID_ARGUMENT;
    }
    r = malloc(sizeof(*r));
    if (!r) {
        return UACPI_STATUS_OUT_OF_MEMORY;
    }
    r->base = (uint16_t)base;
    r->len = (uint16_t)len;
    /* Узкое право пробуем ПЕРВЫМ: если диапазон — порты встроенного контроллера, ходить надо
     * им, а не общим. Проверку «влезает ли» делает ядро, здесь только выбор. */
    r->cap = g_ec_cap != VOID_NO_CAP ? g_ec_cap : g_hw_cap;
    *out = r;
    return UACPI_STATUS_OK;
}

void uacpi_kernel_io_unmap(uacpi_handle h) { free(h); }

static uacpi_status io_read(uacpi_handle h, uacpi_size off, unsigned width, uint32_t *out) {
    struct io_range *r = h;
    uintptr_t v;
    if (off + width > r->len) {
        return UACPI_STATUS_INVALID_ARGUMENT;
    }
    v = vsys_io_read(r->cap, r->base + off, width);
    if (v == VOID_NO_CAP) {
        /* Узкое право могло не покрыть этот порт — тогда пробуем общим, если он есть. Это не
         * обход сужения: оба права выданы нам конфигом, и широкое просто шире. */
        if (r->cap != g_hw_cap && g_hw_cap != VOID_NO_CAP) {
            v = vsys_io_read(g_hw_cap, r->base + off, width);
        }
        if (v == VOID_NO_CAP) {
            SAY_ONCE(ioread, "[acpi] нет права читать порт %#x — область недоступна\n",
                     (unsigned)(r->base + off));
            return UACPI_STATUS_DENIED;
        }
    }
    *out = (uint32_t)v;
    return UACPI_STATUS_OK;
}

static uacpi_status io_write(uacpi_handle h, uacpi_size off, unsigned width, uint32_t val) {
    struct io_range *r = h;
    if (off + width > r->len) {
        return UACPI_STATUS_INVALID_ARGUMENT;
    }
    if (vsys_io_write(r->cap, r->base + off, width, val) != 0) {
        if (r->cap == g_hw_cap || g_hw_cap == VOID_NO_CAP
            || vsys_io_write(g_hw_cap, r->base + off, width, val) != 0) {
            SAY_ONCE(iowrite, "[acpi] нет права писать в порт %#x\n",
                     (unsigned)(r->base + off));
            return UACPI_STATUS_DENIED;
        }
    }
    return UACPI_STATUS_OK;
}

uacpi_status uacpi_kernel_io_read8(uacpi_handle h, uacpi_size off, uacpi_u8 *v) {
    uint32_t t = 0;
    uacpi_status st = io_read(h, off, 1, &t);
    *v = (uacpi_u8)t;
    return st;
}
uacpi_status uacpi_kernel_io_read16(uacpi_handle h, uacpi_size off, uacpi_u16 *v) {
    uint32_t t = 0;
    uacpi_status st = io_read(h, off, 2, &t);
    *v = (uacpi_u16)t;
    return st;
}
uacpi_status uacpi_kernel_io_read32(uacpi_handle h, uacpi_size off, uacpi_u32 *v) {
    uint32_t t = 0;
    uacpi_status st = io_read(h, off, 4, &t);
    *v = t;
    return st;
}
uacpi_status uacpi_kernel_io_write8(uacpi_handle h, uacpi_size off, uacpi_u8 v) {
    return io_write(h, off, 1, v);
}
uacpi_status uacpi_kernel_io_write16(uacpi_handle h, uacpi_size off, uacpi_u16 v) {
    return io_write(h, off, 2, v);
}
uacpi_status uacpi_kernel_io_write32(uacpi_handle h, uacpi_size off, uacpi_u32 v) {
    return io_write(h, off, 4, v);
}

/* ─── конфигурация PCI ────────────────────────────────────────────────────────
 *
 * `SYS_HWPROBE` ходит словами по адресу «шина:устройство.функция»; байтовые и двухбайтовые
 * обращения собираем из слова сами — так же, как это делает всякий драйвер PCI.
 */
struct pci_dev {
    unsigned bdf;
    uacpi_u16 segment;
};

uacpi_status uacpi_kernel_pci_device_open(uacpi_pci_address addr, uacpi_handle *out) {
    struct pci_dev *d;
    if (addr.segment != 0) {
        /* Второй сегмент PCI — это другой корневой мост со своим окном конфигурации, и наш
         * вызов о нём не знает. Молча взять нулевой значило бы читать ЧУЖОЕ устройство. */
        SAY_ONCE(pciseg, "[acpi] сегмент PCI %u не поддержан — устройство пропущено\n",
                 addr.segment);
        return UACPI_STATUS_UNIMPLEMENTED;
    }
    d = malloc(sizeof(*d));
    if (!d) {
        return UACPI_STATUS_OUT_OF_MEMORY;
    }
    d->bdf = ((unsigned)addr.bus << 8) | ((unsigned)addr.device << 3) | addr.function;
    d->segment = addr.segment;
    *out = d;
    return UACPI_STATUS_OK;
}

void uacpi_kernel_pci_device_close(uacpi_handle h) { free(h); }

static uacpi_status pci_word(uacpi_handle h, uacpi_size off, uint32_t *out) {
    struct pci_dev *d = h;
    uintptr_t v = vsys_hw_pci_read(g_hw_cap, d->bdf, off & ~3u);
    if (v == VOID_NO_CAP) {
        SAY_ONCE(pciread, "[acpi] нет права читать конфигурацию PCI\n");
        return UACPI_STATUS_DENIED;
    }
    *out = (uint32_t)v;
    return UACPI_STATUS_OK;
}

uacpi_status uacpi_kernel_pci_read8(uacpi_handle h, uacpi_size off, uacpi_u8 *v) {
    uint32_t w;
    uacpi_status st = pci_word(h, off, &w);
    *v = st == UACPI_STATUS_OK ? (uacpi_u8)(w >> ((off & 3) * 8)) : 0xff;
    return st;
}
uacpi_status uacpi_kernel_pci_read16(uacpi_handle h, uacpi_size off, uacpi_u16 *v) {
    uint32_t w;
    uacpi_status st = pci_word(h, off, &w);
    *v = st == UACPI_STATUS_OK ? (uacpi_u16)(w >> ((off & 2) * 8)) : 0xffff;
    return st;
}
uacpi_status uacpi_kernel_pci_read32(uacpi_handle h, uacpi_size off, uacpi_u32 *v) {
    uint32_t w;
    uacpi_status st = pci_word(h, off, &w);
    *v = st == UACPI_STATUS_OK ? w : 0xffffffff;
    return st;
}

/* Запись уже половины слова — это чтение-правка-запись, и между ними устройство может
 * измениться. Для конфигурации PCI так делают все (другого механизма у шины нет), но сказать
 * об этом надо здесь, а не выяснять потом. */
static uacpi_status pci_write_part(uacpi_handle h, uacpi_size off, uint32_t val, unsigned bytes) {
    struct pci_dev *d = h;
    uint32_t w;
    unsigned shift = (off & 3) * 8;
    uint32_t mask = bytes == 4 ? 0xffffffffu : ((1u << (bytes * 8)) - 1u) << shift;
    if (bytes != 4) {
        uacpi_status st = pci_word(h, off, &w);
        if (st != UACPI_STATUS_OK) {
            return st;
        }
        w = (w & ~mask) | ((val << shift) & mask);
    } else {
        w = val;
    }
    if (vsys_hw_pci_write(g_hw_cap, d->bdf, off & ~3u, w) != 0) {
        SAY_ONCE(pciwrite, "[acpi] нет права писать в конфигурацию PCI\n");
        return UACPI_STATUS_DENIED;
    }
    return UACPI_STATUS_OK;
}

uacpi_status uacpi_kernel_pci_write8(uacpi_handle h, uacpi_size off, uacpi_u8 v) {
    return pci_write_part(h, off, v, 1);
}
uacpi_status uacpi_kernel_pci_write16(uacpi_handle h, uacpi_size off, uacpi_u16 v) {
    return pci_write_part(h, off, v, 2);
}
uacpi_status uacpi_kernel_pci_write32(uacpi_handle h, uacpi_size off, uacpi_u32 v) {
    return pci_write_part(h, off, v, 4);
}

/* ─── отображение физической памяти — ПОКА НЕТ ────────────────────────────────
 *
 * Здесь проходит главная незакрытая граница. Областям AML вида `SystemMemory` (у X54C это
 * NVS прошивки и десятки окон поменьше) нужна настоящая физическая память, а наше право на
 * окно выдаётся под конкретное устройство с шины. Нужен узкий вид права «страницы, которые
 * прошивка объявила своими» — ядро видит карту памяти загрузчика и может его проверить.
 *
 * До тех пор — отказ. Нулями не отвечаем принципиально: прошивка читает из NVS свои флаги, и
 * нули означали бы «графики нет, батареи нет» — правдоподобно и неверно.
 */
/* Куда ложатся отображения прошивки. Между кучей (растёт вверх от 0x6000_0000) и стеком
 * (растёт вниз от 0x8000_0000) — область, в которую не дотянется ни та, ни другой. */
#define FW_VA_BASE 0x70000000u
#define FW_VA_TOP 0x78000000u
#define FW_MAPS_MAX 32
#define PAGE_SIZE 4096u

static struct fw_map {
    uint64_t pa;   /* начало страницы */
    uint64_t pages;
    uintptr_t va;
} g_maps[FW_MAPS_MAX];
static int g_maps_len;
static uintptr_t g_fw_next = FW_VA_BASE;

void *uacpi_kernel_map(uacpi_phys_addr addr, uacpi_size len) {
    /* Таблицы, которые мы собрали сами (см. tables.c), лежат в нашей же памяти, и «физический»
     * адрес у них равен виртуальному. Отличаем их по тому, что они внутри нашей кучи. */
    extern int glue_own_memory(uint64_t addr, uint64_t len);
    if (glue_own_memory(addr, len)) {
        return (void *)(uintptr_t)addr;
    }
    if (g_fw_cap == VOID_NO_CAP) {
        SAY_ONCE(nofw, "[acpi] нет права `fwmem` — память прошивки недоступна\n");
        return NULL;
    }

    uint64_t page = addr & ~(uint64_t)(PAGE_SIZE - 1);
    uint64_t pages = ((addr - page) + len + PAGE_SIZE - 1) / PAGE_SIZE;

    /* Уже отображённое ПЕРЕИСПОЛЬЗУЕМ. uACPI просит одну и ту же область многократно (каждое
     * поле внутри неё — отдельный запрос), и отображать её заново значило бы за десяток полей
     * исчерпать и адреса, и таблицы страниц. */
    for (int i = 0; i < g_maps_len; i++) {
        if (page >= g_maps[i].pa
            && page + pages <= g_maps[i].pa + g_maps[i].pages) {
            return (void *)(g_maps[i].va + (uintptr_t)(addr - g_maps[i].pa));
        }
    }
    if (g_maps_len >= FW_MAPS_MAX || g_fw_next + pages * PAGE_SIZE > FW_VA_TOP) {
        SAY_ONCE(fwfull, "[acpi] место под отображения прошивки кончилось\n");
        return NULL;
    }

    uintptr_t va = g_fw_next;
    /* Ядру отдаём ИСХОДНЫЕ адрес и длину, а не округлённые до страницы. Округлить здесь
     * значило бы обнулить его проверку: таблицы прошивки по страницам не выровнены (FACS —
     * 64 байта), и запрос «страница целиком» не попал бы ни в одну объявленную область.
     * Страницы считает ядро само — ему же и решать, что из них можно отобразить. */
    if (vsys_fw_map(g_fw_cap, addr, len, va) != 0) {
        /* Отказ ядра означает одно из двух: такой области прошивка не объявляла либо не хватило
         * памяти под таблицы страниц. Первое — не наша беда, а свойство машины; врать об этом
         * нулями нельзя, поэтому отдаём отказ дальше. */
        SAY_ONCE(fwdeny, "[acpi] ядро не дало область %#llx (%llu б) — прошивка её не объявляла\n",
                 (unsigned long long)addr, (unsigned long long)len);
        return NULL;
    }
    g_fw_next += pages * PAGE_SIZE;
    g_maps[g_maps_len].pa = page;
    g_maps[g_maps_len].pages = pages;
    g_maps[g_maps_len].va = va;
    g_maps_len++;
    return (void *)(va + (uintptr_t)(addr - page));
}

void uacpi_kernel_unmap(void *addr, uacpi_size len) {
    /* Отображения прошивки не снимаем: их единицы, живут они до конца работы сервера, а
     * возврат страниц потребовал бы обратного вызова к ядру, которого у нас пока нет.
     * Сказано вслух, чтобы это не выглядело забывчивостью. */
    (void)addr;
    (void)len;
}

/* Чтение и запись уже отображённого. Сюда попадают только адреса, которые вернул `map`, то
 * есть наши собственные таблицы; настоящего MMIO пока не бывает. */
uacpi_u8 uacpi_kernel_mmio_read8(void *p) { return *(volatile uacpi_u8 *)p; }
uacpi_u16 uacpi_kernel_mmio_read16(void *p) { return *(volatile uacpi_u16 *)p; }
uacpi_u32 uacpi_kernel_mmio_read32(void *p) { return *(volatile uacpi_u32 *)p; }
uacpi_u64 uacpi_kernel_mmio_read64(void *p) { return *(volatile uacpi_u64 *)p; }
void uacpi_kernel_mmio_write8(void *p, uacpi_u8 v) { *(volatile uacpi_u8 *)p = v; }
void uacpi_kernel_mmio_write16(void *p, uacpi_u16 v) { *(volatile uacpi_u16 *)p = v; }
void uacpi_kernel_mmio_write32(void *p, uacpi_u32 v) { *(volatile uacpi_u32 *)p = v; }
void uacpi_kernel_mmio_write64(void *p, uacpi_u64 v) { *(volatile uacpi_u64 *)p = v; }

/* ─── прерывание SCI — ПОКА НЕТ ───────────────────────────────────────────────
 *
 * Через него приходят события прошивки: нажали кнопку сна, закрыли крышку, контроллер
 * сообщает о батарее. VOID умеет отдавать прерывания пользовательскому процессу
 * (`SYS_IRQ_WAIT` под IRQ-cap) — не хватает только выдачи этого права по вектору из FADT.
 * До тех пор события придётся опрашивать, и об этом честно сказано в журнале.
 */
uacpi_status uacpi_kernel_install_interrupt_handler(uacpi_u32 irq, uacpi_interrupt_handler h,
                                                    uacpi_handle ctx, uacpi_handle *out) {
    *out = (uacpi_handle)1;
    if (g_sci_cap != VOID_NO_CAP) {
        /* Веха 238 — прерывание у нас есть. Запоминаем обработчик: звать его будет наш цикл
         * событий, когда ядро разбудит нас по SCI. Само прерывание ловит ядро — так и должно
         * быть, пользовательский процесс в обработчике прерывания не живёт. */
        g_sci_handler = h;
        g_sci_ctx = ctx;
        printf("[acpi] прерывание прошивки (вектор %u) подключено\n", (unsigned)irq);
        return UACPI_STATUS_OK;
    }
    /* Отвечаем «получилось», хотя обработчик НЕ установлен, и это осознанный выбор между двумя
     * неправдами. Отказ здесь валит подъём целиком (uACPI считает события обязательными), и
     * тогда нет ни батареи, ни крышки — то есть цена «честного» отказа оказывается выше цены
     * потерянных событий.
     *
     * Чтобы это не стало молчаливым враньём, говорим вслух КАЖДУЮ загрузку: события прошивки
     * (крышка, кнопка сна, сообщения контроллера) не придут, пока нет IRQ-права на вектор SCI.
     * Как только оно появится, здесь будет настоящая установка, и эта строка исчезнет. */
    (void)h;
    (void)ctx;
    printf("[acpi] ВНИМАНИЕ: прерывание SCI (вектор %u) не подключено — события прошивки "
           "(крышка, кнопка сна, контроллер) НЕ ПРИДУТ\n",
           (unsigned)irq);
    return UACPI_STATUS_OK;
}

/* Есть ли кого звать по событию. */
int glue_has_sci(void) { return g_sci_handler != NULL; }

/* Разобрать событие прошивки: позвать обработчик uACPI, потом доделать отложенное (запросы
 * контроллера и уведомления она кладёт именно туда). */
void glue_handle_sci(void) {
    if (g_sci_handler) {
        g_sci_handler(g_sci_ctx);
    }
    glue_run_work();
}

uacpi_status uacpi_kernel_uninstall_interrupt_handler(uacpi_interrupt_handler h, uacpi_handle i) {
    (void)h;
    (void)i;
    return UACPI_STATUS_OK;
}

/* ─── запросы прошивки ────────────────────────────────────────────────────────
 *
 * `Breakpoint` и `Fatal` в AML. Первый — отладочный, второй прошивка исполняет, когда считает
 * положение безнадёжным. Молчать нельзя ни о том, ни о другом.
 */
uacpi_status uacpi_kernel_handle_firmware_request(uacpi_firmware_request *req) {
    switch (req->type) {
    case UACPI_FIRMWARE_REQUEST_TYPE_BREAKPOINT:
        printf("[acpi] прошивка просит остановиться (Breakpoint) — продолжаем\n");
        break;
    case UACPI_FIRMWARE_REQUEST_TYPE_FATAL:
        printf("[acpi] прошивка объявила сбой: тип %u, код %u, аргумент %llu\n",
               (unsigned)req->fatal.type, (unsigned)req->fatal.code,
               (unsigned long long)req->fatal.arg);
        break;
    default:
        break;
    }
    return UACPI_STATUS_OK;
}

/* ─── подъём и спуск ──────────────────────────────────────────────────────────
 *
 * uACPI зовёт их на каждой ступени своей инициализации — место, где система могла бы
 * доделать что-то своё. Нам пока нечего.
 */
uacpi_status uacpi_kernel_initialize(uacpi_init_level lvl) {
    (void)lvl;
    return UACPI_STATUS_OK;
}

void uacpi_kernel_deinitialize(void) {}
