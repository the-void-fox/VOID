# lx-linux — неизменённый код ядра Linux на VOID (Вехи 55–56, dde_linux-конвейер)

Порт реальных `.c` из ядра Linux: неизменённый файл ядра компилируется против рукописных
шим-заголовков `linux/*.h` («lx_emul-заголовки») + C-рантайма `lx_kit.c` (Lx_kit) и работает на
VOID. Растёт по мере роста портируемого кода к настоящему драйверу (полигон — e1000).

- **Веха 55** — пилот конвейера: `lib/sort.c` (heapsort, ничего не аллоцирует) → `lx-sort`.
- **Веха 56** — фундамент **Lx_kit**: аллокатор ядра (`kmalloc`-семейство) + header-поверхность;
  первый `.c`, честно аллоцирующий память ядровым `kmalloc` — `lib/argv_split.c` → `lx-argv`.

## Что здесь

Vendored (**НЕИЗМЕНЁННЫЕ**, verbatim из Linux **6.18.7**, GPL-2.0, SPDX на месте):
- **`linux-src/sort.c`** + **`linux/sort.h`** — `lib/sort.c` и `include/linux/sort.h` (Веха 55).
- **`linux-src/argv_split.c`** — `lib/argv_split.c` (Веха 56).

Наши шим-заголовки `linux/*.h` (lx_emul: дают ядровому коду ровно тот API, что он ждёт, под VOID):
- `types.h`, `export.h`, `sched.h` — минимум под sort.c (Веха 55).
- `kernel.h`, `slab.h`, `gfp.h`, `string.h`, `ctype.h`, `printk.h` — под argv_split.c (Веха 56):
  аллокатор/строки/классификация/печать/идиомы. Флаги `gfp_t` игнорируются (куча единая).

Наш рантайм и харнессы:
- **`lx_kit.c`** — **Lx_kit-рантайм**: тела `kmalloc/kzalloc/kcalloc/kmalloc_array/krealloc/kfree` +
  `kmemdup/kstrdup/kstrndup` над кучей newlib + `printk`. Растёт к таймерам/workqueue/ioremap/DMA.
- **`main.c`** — харнесс sort; **`main_argv.c`** — харнесс argv_split (проверка + печать `printk`).

Лицензии: vendored-файлы Linux остаются под GPL-2.0 (свои SPDX-заголовки); шимы, рантайм и харнессы —
код проекта. Хостинг Linux-драйверов по природе смешивает лицензии (портируемые части — GPL).

## Сборка (C-мир: nix-build → мост, как lx_e1000)

```
nix-build nix -A <arch>.lx_sort    # sort.c + шимы + harness → VOID-ELF (<arch> = x86_64 | riscv64)
nix-build nix -A <arch>.lx_argv    # argv_split.c + шимы + lx_kit.c + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-sort bin/<arch>/lx-sort
void-store-import void-disk.img put result/bin/lx-argv bin/<arch>/lx-argv
```
Запуск в vsh: `run bin/lx-sort` / `run bin/lx-argv` (чистые вычислялки, обе архитектуры).
