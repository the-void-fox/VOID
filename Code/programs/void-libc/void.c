/* void-libc (Веха 36) — глю между newlib и VOID: старт процесса + syscall-стабы.
 *
 * newlib даёт весь «верх» libc (stdio, malloc, string, качественные printf/scanf) и
 * ждёт от платформы классическую дюжину стабов (_write/_read/_open/_sbrk/_exit/…) —
 * ровно тот контракт, под который ОС-порты newlib пишутся десятилетиями. Здесь эти
 * стабы переводятся в мир VOID тем же путём, что std-порт Rust (sys/pal/void):
 *
 * - консоль: fd 0 → SYS_READ, fd 1/2 → SYS_WRITE (ядро, минуя персоналию);
 * - файлы: IPC к посикс-персоналии (эндпоинт — стартовый capability слот 0,
 *   «preopen»-модель Вехи 30), op-коды и границы буферов — programs/user/src/bin/posixfs.rs;
 * - куча: ОДИН ленивый SYS_MAP на старте, _sbrk двигает границу внутри резерва —
 *   физические страницы придут по page fault только под реально тронутое;
 * - argv/env: SYS_ARGS(0/1), NUL-разделённые блобы → вектора argc/argv/environ.
 *
 * Однопоточно (нити C-миру не обещаны — это дорожка std/rayon); файлы — плоские имена
 * до 4 КиБ (ограничения персоналии, там же).
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

#include "syscall.h"

/* ─── состояние процесса ─────────────────────────────────────────────────────── */

#define HEAP_RESERVE (16u * 1024 * 1024)
#define FD_BASE 3      /* 0/1/2 — консоль; дальше — дескрипторы персоналии */
#define NFILES 16      /* лимит персоналии: fd и слоты файлов */
#define NAME_MAX_ 32   /* лимит персоналии на имя */

static uintptr_t fs_ep = VOID_NO_CAP; /* эндпоинт posixfs (старт-cap слот 0) */
static char *brk_cur, *brk_end;

/* Имя за каждым открытым fd — для _fstat: у персоналии stat только по имени. */
static char fd_names[FD_BASE + NFILES][NAME_MAX_ + 1];

char **environ;

/* ─── старт: куча → argv/env → конструкторы → main ──────────────────────────── */

extern int main(int argc, char **argv, char **envp);

typedef void (*initfn)(void);
extern initfn __init_array_start[], __init_array_end[];
extern initfn __fini_array_start[], __fini_array_end[];

static char argbuf[512], envbuf[512];
static char *argvec[34], *envvec[34];

/* NUL-разделённый блоб (`a\0b\0c`, БЕЗ хвостового NUL) → NULL-терминированный
 * вектор указателей. Возвращает число записей. */
static int split_blob(char *buf, size_t len, char **vec, int cap) {
    int n = 0;
    size_t off = 0;
    buf[len] = 0; /* дотерминировать последнюю запись (буфер на байт больше len) */
    while (off < len && n < cap) {
        vec[n++] = buf + off;
        off += strlen(buf + off) + 1;
    }
    vec[n] = 0;
    return n;
}

static void run_fini_array(void) {
    for (initfn *f = __fini_array_end; f != __fini_array_start;)
        (*--f)();
}

__attribute__((noreturn)) void _start_c(void) {
    uintptr_t heap = vsys_map(HEAP_RESERVE);
    if (heap != VOID_NO_CAP) {
        brk_cur = (char *)heap;
        brk_end = brk_cur + HEAP_RESERVE;
    }
    fs_ep = vsys_start_cap(0);

    size_t an = vsys_args(0, argbuf, sizeof(argbuf) - 1);
    if (an > sizeof(argbuf) - 1)
        an = sizeof(argbuf) - 1;
    int argc = split_blob(argbuf, an, argvec, 33);

    size_t en = vsys_args(1, envbuf, sizeof(envbuf) - 1);
    if (en > sizeof(envbuf) - 1)
        en = sizeof(envbuf) - 1;
    split_blob(envbuf, en, envvec, 33);
    environ = envvec;

    atexit(run_fini_array);
    for (initfn *f = __init_array_start; f != __init_array_end; f++)
        (*f)();

    exit(main(argc, argvec, environ));
}

/* ─── помощники IPC ──────────────────────────────────────────────────────────── */

/* op-код персоналии: операция | fd | режим (см. posixfs.rs). fd здесь — её fd (без FD_BASE). */
static uintptr_t fs_op(unsigned op, unsigned fd, unsigned mode) {
    return op | (uintptr_t)fd << 8 | (uintptr_t)mode << 16;
}

/* stat по имени: 1 — есть (размер в *size), 0 — нет, -1 — персоналия недоступна. */
static int fs_stat(const char *name, uint32_t *size) {
    unsigned char rep[5] = {0};
    if (fs_ep == VOID_NO_CAP)
        return -1;
    uintptr_t n = vsys_call(fs_ep, fs_op(VOID_OP_STAT, 0, 0), name, strlen(name), rep,
                            sizeof(rep));
    if (n == VOID_NO_CAP || n < 5)
        return -1;
    if (!rep[0])
        return 0;
    *size = (uint32_t)rep[1] | (uint32_t)rep[2] << 8 | (uint32_t)rep[3] << 16 |
            (uint32_t)rep[4] << 24;
    return 1;
}

static int bad_fd(int fd) {
    return fd < FD_BASE || fd >= FD_BASE + NFILES || !fd_names[fd][0];
}

/* ─── стабы newlib ───────────────────────────────────────────────────────────── */

void _exit(int code) {
    vsys_exit((uintptr_t)(code & 0xff));
}

int _open(const char *name, int flags, ...) {
    unsigned char rep[1] = {0xff};
    size_t nl = strlen(name);
    if (fs_ep == VOID_NO_CAP) {
        errno = ENODEV;
        return -1;
    }
    if (nl == 0 || nl > NAME_MAX_) {
        errno = ENAMETOOLONG;
        return -1;
    }
    /* Персоналия создаёт файл при ЛЮБОМ open — POSIX-семантику O_CREAT держим здесь:
     * без него открывать несуществующее нельзя (fopen("нет","r") обязан дать NULL). */
    if (!(flags & O_CREAT)) {
        uint32_t sz;
        int e = fs_stat(name, &sz);
        if (e <= 0) {
            errno = e ? EIO : ENOENT;
            return -1;
        }
    }
    unsigned mode = 0;
    if (flags & O_TRUNC)
        mode |= VOID_O_TRUNC;
    if (flags & O_APPEND)
        mode |= VOID_O_APPEND;
    uintptr_t n = vsys_call(fs_ep, fs_op(VOID_OP_OPEN, 0, mode), name, nl, rep, 1);
    if (n == VOID_NO_CAP || rep[0] == 0xff) {
        errno = EMFILE; /* слоты персоналии кончились */
        return -1;
    }
    int fd = rep[0] + FD_BASE;
    strcpy(fd_names[fd], name);
    return fd;
}

int _close(int fd) {
    if (fd < FD_BASE)
        return 0;
    if (bad_fd(fd)) {
        errno = EBADF;
        return -1;
    }
    vsys_call(fs_ep, fs_op(VOID_OP_CLOSE, fd - FD_BASE, 0), 0, 0, 0, 0);
    fd_names[fd][0] = 0;
    return 0;
}

_READ_WRITE_RETURN_TYPE _write(int fd, const void *buf, size_t len) {
    const char *p = buf;
    if (fd == 1 || fd == 2) {
        vsys_write(buf, len);
        return (_READ_WRITE_RETURN_TYPE)len;
    }
    if (bad_fd(fd)) {
        errno = EBADF;
        return -1;
    }
    /* Запросы к персоналии режутся её буфером — пишем кусками, как fs-слой std. */
    for (size_t off = 0; off < len;) {
        size_t n = len - off;
        if (n > VOID_IPC_MAX)
            n = VOID_IPC_MAX;
        vsys_call(fs_ep, fs_op(VOID_OP_WRITE, fd - FD_BASE, 0), p + off, n, 0, 0);
        off += n;
    }
    return (_READ_WRITE_RETURN_TYPE)len;
}

_READ_WRITE_RETURN_TYPE _read(int fd, void *buf, size_t len) {
    if (fd == 0)
        return (_READ_WRITE_RETURN_TYPE)vsys_read_stdin(buf, len); /* блокируется до ≥1 байта */
    if (bad_fd(fd)) {
        errno = EBADF;
        return -1;
    }
    /* Короткое чтение (≤512) законно — stdio сам дочитает. */
    size_t want = len > VOID_IPC_MAX ? VOID_IPC_MAX : len;
    uintptr_t n = vsys_call(fs_ep, fs_op(VOID_OP_READ, fd - FD_BASE, 0), 0, 0, buf, want);
    if (n == VOID_NO_CAP) {
        errno = EIO;
        return -1;
    }
    return (_READ_WRITE_RETURN_TYPE)n;
}

_off_t _lseek(int fd, _off_t off, int whence) {
    if (fd < FD_BASE)
        return 0; /* консоль: не seekable, но и не ошибка (stdio дёргает на fflush) */
    if (bad_fd(fd) || whence < 0 || whence > 2) {
        errno = EBADF;
        return -1;
    }
    int64_t req = off;
    unsigned char rep[8] = {0};
    uintptr_t n = vsys_call(fs_ep, fs_op(VOID_OP_SEEK, fd - FD_BASE, (unsigned)whence),
                            &req, 8, rep, 8);
    uint64_t pos = 0;
    for (int i = 7; i >= 0; i--)
        pos = pos << 8 | rep[i];
    if (n == VOID_NO_CAP || n < 8 || pos == UINT64_MAX) {
        errno = EINVAL;
        return -1;
    }
    return (_off_t)pos;
}

int _fstat(int fd, struct stat *st) {
    memset(st, 0, sizeof(*st));
    if (fd < FD_BASE) {
        st->st_mode = S_IFCHR; /* терминал → stdio возьмёт построчную буферизацию */
        return 0;
    }
    if (bad_fd(fd)) {
        errno = EBADF;
        return -1;
    }
    uint32_t sz = 0;
    if (fs_stat(fd_names[fd], &sz) <= 0) {
        /* открыт, но ещё не закрыт ни разу (нет корня) — честный пустой файл */
        sz = 0;
    }
    st->st_mode = S_IFREG | 0644;
    st->st_size = sz;
    st->st_blksize = 512;
    return 0;
}

int _stat(const char *name, struct stat *st) {
    uint32_t sz = 0;
    int e = fs_stat(name, &sz);
    if (e <= 0) {
        errno = e ? EIO : ENOENT;
        return -1;
    }
    memset(st, 0, sizeof(*st));
    st->st_mode = S_IFREG | 0644;
    st->st_size = sz;
    st->st_blksize = 512;
    return 0;
}

int _unlink(const char *name) {
    unsigned char rep[1];
    if (fs_ep == VOID_NO_CAP) {
        errno = ENODEV;
        return -1;
    }
    vsys_call(fs_ep, fs_op(VOID_OP_UNLINK, 0, 0), name, strlen(name), rep, 1);
    return 0;
}

/* Честный rename персоналии (Веха 30): old_len(1) | old | new. */
int _rename(const char *oldn, const char *newn) {
    unsigned char req[1 + 2 * NAME_MAX_], rep[1] = {0xff};
    size_t ol = strlen(oldn), nl = strlen(newn);
    if (fs_ep == VOID_NO_CAP || !ol || !nl || ol > NAME_MAX_ || nl > NAME_MAX_) {
        errno = EINVAL;
        return -1;
    }
    req[0] = (unsigned char)ol;
    memcpy(req + 1, oldn, ol);
    memcpy(req + 1 + ol, newn, nl);
    vsys_call(fs_ep, fs_op(VOID_OP_RENAME, 0, 0), req, 1 + ol + nl, rep, 1);
    if (rep[0]) {
        errno = ENOENT;
        return -1;
    }
    return 0;
}

int _isatty(int fd) {
    if (fd < FD_BASE)
        return 1;
    errno = ENOTTY;
    return 0;
}

void *_sbrk(ptrdiff_t inc) {
    if (!brk_cur || brk_cur + inc > brk_end || brk_cur + inc < brk_end - HEAP_RESERVE) {
        errno = ENOMEM;
        return (void *)-1;
    }
    char *p = brk_cur;
    brk_cur += inc;
    return p;
}

int _getpid(void) {
    return 1;
}

/* Сюда приходит raise()/abort() newlib (сигналов у VOID нет) — упасть честным кодом. */
int _kill(int pid, int sig) {
    (void)pid;
    vsys_exit(128u + (unsigned)sig);
}

int _gettimeofday(struct timeval *tv, void *tz) {
    (void)tz;
    uint64_t ns = vsys_ticks() * VOID_TICK_NS; /* от загрузки: базы календаря нет, как в std */
    tv->tv_sec = (time_t)(ns / 1000000000u);
    tv->tv_usec = (suseconds_t)(ns % 1000000000u / 1000u);
    return 0;
}

int _link(const char *o, const char *n) {
    (void)o;
    (void)n;
    errno = ENOSYS;
    return -1;
}

/* dup/неблокирующих режимов у персоналии нет; наличие символа выключает
 * gnulib-замену fcntl (та тянет модуль dupfd, которого нет в minimal-наборах). */
int _fcntl(int fd, int cmd, ...) {
    (void)fd;
    (void)cmd;
    errno = ENOSYS;
    return -1;
}

/* ─── имена без подчёркивания ────────────────────────────────────────────────
 * newlib x86_64-elf собран под ГОЛЫЕ имена syscall'ов (write), riscv64-none-elf —
 * под подчёркнутые (_write). Алиасы дают оба варианта из одного тела; заодно это
 * честные POSIX-обёртки для программ, зовущих open/read/write напрямую (bzip2).
 * Одноимённые объекты libc.a при этом просто не подтягиваются (символ уже есть). */
#define VOID_ALIAS(name) extern __typeof(_##name) name __attribute__((alias("_" #name)))
VOID_ALIAS(open);
VOID_ALIAS(close);
VOID_ALIAS(write);
VOID_ALIAS(read);
VOID_ALIAS(lseek);
VOID_ALIAS(fstat);
VOID_ALIAS(stat);
VOID_ALIAS(unlink);
VOID_ALIAS(rename);
VOID_ALIAS(isatty);
VOID_ALIAS(sbrk);
VOID_ALIAS(getpid);
VOID_ALIAS(kill);
VOID_ALIAS(gettimeofday);
VOID_ALIAS(link);
VOID_ALIAS(fcntl);
/* Симлинков у персоналии нет — lstat совпадает со stat (ждёт bzip2). */
extern __typeof(_stat) lstat __attribute__((alias("_stat")));

/* ─── POSIX-мелочь, которой нет в newlib, но которую ждут пакеты (bzip2) ─────── */

int getdtablesize(void) {
    return FD_BASE + NFILES; /* 3 консольных + слоты персоналии */
}

const char *getprogname(void) {
    const char *p = argvec[0] ? argvec[0] : "";
    const char *s = strrchr(p, '/');
    return s ? s + 1 : p;
}

int chmod(const char *path, mode_t mode) {
    (void)path;
    (void)mode;
    return 0; /* прав у персоналии нет — молча соглашаемся */
}

int chown(const char *path, uid_t o, gid_t g) {
    (void)path;
    (void)o;
    (void)g;
    return 0;
}

int fchmod(int fd, mode_t mode) {
    (void)fd;
    (void)mode;
    return 0;
}

int fchown(int fd, uid_t o, gid_t g) {
    (void)fd;
    (void)o;
    (void)g;
    return 0;
}

int utime(const char *path, const void *times) {
    (void)path;
    (void)times;
    return 0; /* mtime у персоналии нет */
}
