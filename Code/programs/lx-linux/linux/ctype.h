/* linux/ctype.h — ШИМ lx_emul (Веха 56), НЕ исходник Linux.
 *
 * Классификация символов. В ядре — таблица _ctype[] (lib/ctype.c); мы даём эквивалентные
 * static inline по ASCII (та же семантика для 7-битных значений, что и ждёт код ядра). Свои имена,
 * не тянем newlib <ctype.h>, чтобы не конфликтовать. Растёт по мере надобности портируемого кода.
 */
#ifndef _LINUX_CTYPE_H_SHIM
#define _LINUX_CTYPE_H_SHIM

static inline int isspace(int c)  { return c == ' ' || (c >= '\t' && c <= '\r'); }
static inline int isdigit(int c)  { return c >= '0' && c <= '9'; }
static inline int isupper(int c)  { return c >= 'A' && c <= 'Z'; }
static inline int islower(int c)  { return c >= 'a' && c <= 'z'; }
static inline int isalpha(int c)  { return isupper(c) || islower(c); }
static inline int isalnum(int c)  { return isalpha(c) || isdigit(c); }
static inline int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
static inline int isprint(int c)  { return c >= 0x20 && c < 0x7f; }
static inline int toupper(int c)  { return islower(c) ? c - 'a' + 'A' : c; }
static inline int tolower(int c)  { return isupper(c) ? c - 'A' + 'a' : c; }

#endif /* _LINUX_CTYPE_H_SHIM */
