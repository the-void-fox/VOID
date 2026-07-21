/* Декларации функций void-libc, которых нет в заголовках newlib (Веха 36).
 * Подключается кросс-сборке пакетов через `-include` (см. nix/default.nix):
 * gnulib проверяет наличие ФУНКЦИИ линковкой, а объявления ждёт от stdlib.h —
 * newlib его не даёт, и «функция есть, декларации нет» роняет сборку. */
#pragma once

extern const char *getprogname(void);
extern int getdtablesize(void);
