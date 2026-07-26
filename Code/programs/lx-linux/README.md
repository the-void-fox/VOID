# lx-linux — неизменённый код ядра Linux на VOID (Веха 55, dde_linux-конвейер)

Первый шаг порта реальных `.c` из ядра Linux: неизменённый файл ядра компилируется против
рукописных шим-заголовков `linux/*.h` (начало «lx_emul-заголовков») и работает на VOID. Пилот —
`lib/sort.c` (heapsort). Растёт по мере роста портируемого кода к настоящему драйверу.

## Что здесь

- **`linux-src/sort.c`** — **НЕИЗМЕНЁННЫЙ** `lib/sort.c` из Linux **6.18.7** (GPL-2.0, SPDX-заголовок
  на месте). Не редактируется — берётся verbatim из дерева ядра.
- **`linux/sort.h`** — **НЕИЗМЕНЁННЫЙ** `include/linux/sort.h` из Linux 6.18.7 (GPL-2.0).
- **`linux/types.h`, `linux/export.h`, `linux/sched.h`** — НАШИ шим-заголовки (lx_emul): дают
  ядровому коду ровно те типы/макросы, что он ждёт, но реализованные под VOID. Минимум под sort.c.
- **`main.c`** — НАШ харнесс: зовёт `sort()` и проверяет результат.

Лицензии: vendored-файлы Linux остаются под GPL-2.0 (свои SPDX-заголовки); шимы и харнесс — код
проекта. Хостинг Linux-драйверов по природе смешивает лицензии (портируемые части — GPL).

## Сборка (C-мир: nix-build → мост, как lx_e1000)

```
nix-build nix -A x86_64.lx_sort    # кросс-gcc против void-libc + шим-заголовков → VOID-ELF
nix-build nix -A riscv64.lx_sort
void-store-import void-disk.img put result/bin/lx-sort bin/<arch>/lx-sort
```
Запуск в vsh: `run bin/lx-sort` (чистая вычислялка, работает на обеих архитектурах).
