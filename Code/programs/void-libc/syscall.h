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
