/* Syscall-ABI VOID для C-мира (Веха 36) — зеркало Code/programs/user/src/lib.rs.
 *
 * Номера и раскладка регистров обязаны совпадать с kernel/src/proc.rs и
 * kernel/src/arch/{riscv64,x86_64}/trap.rs: riscv64 — номер в a7, аргументы a0..a6,
 * результат a0 (`ecall`); x86_64 — номер в rax, аргументы rdi,rsi,rdx,r10,r8,r9,rbx,
 * результат rax (`int 0x80`). В отличие от Rust-шимов, здесь можно назвать rbx
 * операндом напрямую (ограничение "b" — это GCC, резерв rbx есть только у LLVM).
 */
#pragma once
#include <stddef.h>
#include <stdint.h>

#define SYS_WRITE 1
#define SYS_EXIT 2
#define SYS_YIELD 3
#define SYS_CALL 5
#define SYS_READ 14
#define SYS_MAP 17
#define SYS_ARGS 18
#define SYS_STARTCAP 19
/* Веха 54 — драйверные syscall'ы для C-мира (нити + фундамент userspace-драйверов Вех 51–52). */
#define SYS_THREAD_SPAWN 23
#define SYS_THREAD_EXIT 24
#define SYS_FUTEX 26
#define SYS_MMIO_MAP 31
#define SYS_DMA_ALLOC 32
#define SYS_IRQ_WAIT 33
#define SYS_SLEEP 47
#define SYS_RANDOM 37

/* «Права нет» — и в аргументе capability SYS_CALL, и в ответе SYS_STARTCAP. */
#define VOID_NO_CAP ((uintptr_t)-1)

/* op-коды посикс-персоналии (см. programs/user/src/bin/posixfs.rs):
 * операция (младший байт) | fd (байт 8..16) | режим (байт 16..24). */
#define VOID_OP_OPEN 0
#define VOID_OP_READ 1
#define VOID_OP_WRITE 2
#define VOID_OP_CLOSE 3
#define VOID_OP_STAT 4
#define VOID_OP_UNLINK 5
#define VOID_OP_READDIR 6
#define VOID_OP_SEEK 7
#define VOID_OP_RENAME 8

#define VOID_O_APPEND (1u << 0)
#define VOID_O_TRUNC (1u << 1)

/* Ответы персоналии не длиннее её буфера — резать запросы по этой границе. */
#define VOID_IPC_MAX 512

#if defined(__riscv)

static inline uintptr_t vsys(uintptr_t n, uintptr_t a0, uintptr_t a1, uintptr_t a2,
                             uintptr_t a3, uintptr_t a4, uintptr_t a5, uintptr_t a6) {
    register uintptr_t rn asm("a7") = n;
    register uintptr_t r0 asm("a0") = a0;
    register uintptr_t r1 asm("a1") = a1;
    register uintptr_t r2 asm("a2") = a2;
    register uintptr_t r3 asm("a3") = a3;
    register uintptr_t r4 asm("a4") = a4;
    register uintptr_t r5 asm("a5") = a5;
    register uintptr_t r6 asm("a6") = a6;
    asm volatile("ecall"
                 : "+r"(r0), "+r"(r1), "+r"(r2), "+r"(r3)
                 : "r"(rn), "r"(r4), "r"(r5), "r"(r6)
                 : "memory");
    return r0;
}

static inline __attribute__((noreturn)) void vsys_exit(uintptr_t code) {
    register uintptr_t rn asm("a7") = SYS_EXIT;
    register uintptr_t r0 asm("a0") = code;
    asm volatile("ecall" : : "r"(rn), "r"(r0) : "memory");
    __builtin_unreachable();
}

/* Монотонные тики из U-mode (не syscall): rdtime, таймбаза QEMU virt 10 МГц. */
#define VOID_TICK_NS 100
static inline uint64_t vsys_ticks(void) {
    uint64_t t;
    asm volatile("rdtime %0" : "=r"(t));
    return t;
}

#elif defined(__x86_64__)

static inline uintptr_t vsys(uintptr_t n, uintptr_t a0, uintptr_t a1, uintptr_t a2,
                             uintptr_t a3, uintptr_t a4, uintptr_t a5, uintptr_t a6) {
    register uintptr_t r10 asm("r10") = a3;
    register uintptr_t r8 asm("r8") = a4;
    register uintptr_t r9 asm("r9") = a5;
    asm volatile("int $0x80"
                 : "+a"(n), "+D"(a0), "+S"(a1), "+d"(a2)
                 : "b"(a6), "r"(r10), "r"(r8), "r"(r9)
                 : "memory", "cc");
    return n;
}

static inline __attribute__((noreturn)) void vsys_exit(uintptr_t code) {
    asm volatile("int $0x80" : : "a"((uintptr_t)SYS_EXIT), "D"(code) : "memory");
    __builtin_unreachable();
}

/* Монотонные тики из U-mode: rdtsc (~1 ГГц в QEMU TCG; на железе пересчитать). */
#define VOID_TICK_NS 1
static inline uint64_t vsys_ticks(void) {
    uint32_t lo, hi;
    asm volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
}

#else
#error "VOID C-мир: поддержаны только riscv64 и x86_64"
#endif

/* Удобные обёртки — сигнатуры повторяют шимы Rust. */
static inline void vsys_write(const void *buf, size_t len) {
    vsys(SYS_WRITE, (uintptr_t)buf, len, 0, 0, 0, 0, 0);
}
static inline size_t vsys_read_stdin(void *buf, size_t len) {
    return vsys(SYS_READ, (uintptr_t)buf, len, 0, 0, 0, 0, 0);
}
static inline uintptr_t vsys_map(size_t len) {
    return vsys(SYS_MAP, len, 0, 0, 0, 0, 0, 0);
}
static inline size_t vsys_args(uintptr_t sel, void *buf, size_t len) {
    return vsys(SYS_ARGS, sel, (uintptr_t)buf, len, 0, 0, 0, 0);
}
static inline uintptr_t vsys_start_cap(uintptr_t i) {
    return vsys(SYS_STARTCAP, i, 0, 0, 0, 0, 0, 0);
}
/* SYS_CALL без передачи права: байты ответа (усечены по recv) или VOID_NO_CAP. */
static inline uintptr_t vsys_call(uintptr_t ep, uintptr_t op, const void *send,
                                  size_t send_len, void *recv, size_t recv_len) {
    return vsys(SYS_CALL, ep, op, (uintptr_t)send, send_len, (uintptr_t)recv, recv_len,
                VOID_NO_CAP);
}

/* ─── драйверные syscall'ы (Веха 54): нити + MMIO/DMA/IRQ фундамента Вех 51–52 ─── */

/* SYS_THREAD_SPAWN(entry, arg, stack_top) -> tid | VOID_NO_CAP: нить в том же простор/домене. */
static inline uintptr_t vsys_thread_spawn(uintptr_t entry, uintptr_t arg, uintptr_t stack_top) {
    return vsys(SYS_THREAD_SPAWN, entry, arg, stack_top, 0, 0, 0, 0);
}
/* SYS_THREAD_EXIT(retval): завершить ТЕКУЩУЮ нить (процесс живёт прочими нитями). */
static inline __attribute__((noreturn)) void vsys_thread_exit(uintptr_t retval) {
    vsys(SYS_THREAD_EXIT, retval, 0, 0, 0, 0, 0, 0);
    __builtin_unreachable();
}
/* SYS_FUTEX WAIT: уснуть на *uaddr, пока == expected. timeout_ticks=0 — бессрочно. 0=разбужен,1=таймаут. */
static inline uintptr_t vsys_futex_wait(const uint32_t *uaddr, uint32_t expected,
                                        uintptr_t timeout_ticks) {
    return vsys(SYS_FUTEX, 0, (uintptr_t)uaddr, expected, timeout_ticks, 0, 0, 0);
}
/* SYS_FUTEX WAKE: разбудить до count нитей на *uaddr; возврат — число разбуженных. */
static inline uintptr_t vsys_futex_wake(const uint32_t *uaddr, uintptr_t count) {
    return vsys(SYS_FUTEX, 1, (uintptr_t)uaddr, count, 0, 0, 0, 0);
}
/* SYS_MMIO_MAP(cap, va): замапить окно регистров устройства в свой простор. 1 — успех. */
static inline int vsys_mmio_map(uintptr_t cap, uintptr_t va) {
    return vsys(SYS_MMIO_MAP, cap, va, 0, 0, 0, 0, 0) == 0;
}
/* SYS_DMA_ALLOC(cap, va): DMA-страница по va, возврат — её ФИЗ-адрес (VOID_NO_CAP — отказ). */
static inline uintptr_t vsys_dma_alloc(uintptr_t cap, uintptr_t va) {
    return vsys(SYS_DMA_ALLOC, cap, va, 0, 0, 0, 0, 0);
}
/* То же на `pages` ПОДРЯД идущих страниц (Веха 133): кольцо дескрипторов карта обходит сама, о
 * таблицах страниц не зная, — значит физическая непрерывность обязательна, и набрать её
 * несколькими вызовами по странице нельзя. */
static inline uintptr_t vsys_dma_alloc_n(uintptr_t cap, uintptr_t va, size_t pages) {
    return vsys(SYS_DMA_ALLOC, cap, va, pages, 0, 0, 0, 0);
}
/* SYS_IRQ_WAIT(cap): уснуть до прерывания устройства. 1 — проснулись по IRQ, 0 — нет права. */
static inline int vsys_irq_wait(uintptr_t cap) {
    return vsys(SYS_IRQ_WAIT, cap, 0, 0, 0, 0, 0, 0) == 0;
}
/* SYS_RANDOM(buf, len): случайные байты от ядра (аппаратный ГСЧ + пул событий). Права не
 * требует: случайность — не ресурс, а свойство системы. Веха 131 — понадобился драйверу:
 * сетевые карты без MAC в EEPROM обязаны сгенерировать себе адрес. */
static inline void vsys_random(void *buf, size_t len) {
    vsys(SYS_RANDOM, (uintptr_t)buf, len, 0, 0, 0, 0, 0);
}
/* SYS_SLEEP(ns): поспать и НЕ занимать процессор. Веха 134 — понадобился холостому ходу
 * планировщика Lx_kit: он ждал срок таймера глухим циклом, и долгоживущий драйвер съедал бы
 * свою долю процессора вечно. */
static inline void vsys_sleep_ns(unsigned long long ns) {
    vsys(SYS_SLEEP, (uintptr_t)ns, 0, 0, 0, 0, 0, 0);
}
