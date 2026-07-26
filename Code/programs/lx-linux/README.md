# lx-linux — неизменённый код ядра Linux на VOID (Вехи 55–57, dde_linux-конвейер)

Порт реальных `.c` из ядра Linux: неизменённый файл ядра компилируется против рукописных
шим-заголовков `linux/*.h` («lx_emul-заголовки») + C-рантайма `lx_kit.c` (Lx_kit) и работает на
VOID. Растёт по мере роста портируемого кода к настоящему драйверу (полигон — e1000).

- **Веха 55** — пилот конвейера: `lib/sort.c` (heapsort, ничего не аллоцирует) → `lx-sort`.
- **Веха 56** — фундамент **Lx_kit**: аллокатор ядра (`kmalloc`-семейство) + header-поверхность;
  первый `.c`, честно аллоцирующий память ядровым `kmalloc` — `lib/argv_split.c` → `lx-argv`.
- **Веха 57** — **`linux/list.h`**: двусвязный список ядра (костяк драйверов); реальный
  `lib/list_sort.c` (merge-сортировка списка) → `lx-list`.

## Что здесь

Vendored (**НЕИЗМЕНЁННЫЕ**, verbatim из Linux **6.18.7**, GPL-2.0, SPDX на месте):
- **`linux-src/sort.c`** + **`linux/sort.h`** — `lib/sort.c` и `include/linux/sort.h` (Веха 55).
- **`linux-src/argv_split.c`** — `lib/argv_split.c` (Веха 56).
- **`linux-src/list_sort.c`** + **`linux/list_sort.h`** — `lib/list_sort.c` и `include/linux/list_sort.h` (Веха 57).

Наши шим-заголовки `linux/*.h` (lx_emul: дают ядровому коду ровно тот API, что он ждёт, под VOID):
- `types.h`, `export.h`, `sched.h` — минимум под sort.c (Веха 55).
- `kernel.h`, `slab.h`, `gfp.h`, `string.h`, `ctype.h`, `printk.h` — под argv_split.c (Веха 56):
  аллокатор/строки/классификация/печать/идиомы. Флаги `gfp_t` игнорируются (куча единая).
- `list.h`, `compiler.h`, `container_of.h` — под list_sort.c (Веха 57): двусвязный список +
  likely/unlikely; `container_of` вынесен в свой заголовок (как в ядре 6.x).

Наш рантайм и харнессы:
- **`lx_kit.c`** — **Lx_kit-рантайм**: тела `kmalloc/kzalloc/kcalloc/kmalloc_array/krealloc/kfree` +
  `kmemdup/kstrdup/kstrndup` над кучей newlib + `printk`. Растёт к таймерам/workqueue/ioremap/DMA.
- **`main.c`** — харнесс sort; **`main_argv.c`** — argv_split; **`main_list.c`** — list_sort
  (строит список, сортирует, проверяет порядок + целостность кольца).

Лицензии: vendored-файлы Linux остаются под GPL-2.0 (свои SPDX-заголовки); шимы, рантайм и харнессы —
код проекта. Хостинг Linux-драйверов по природе смешивает лицензии (портируемые части — GPL).

## Сборка (C-мир: nix-build → мост, как lx_e1000)

```
nix-build nix -A <arch>.lx_sort    # sort.c + шимы + harness → VOID-ELF (<arch> = x86_64 | riscv64)
nix-build nix -A <arch>.lx_argv    # argv_split.c + шимы + lx_kit.c + harness → VOID-ELF
nix-build nix -A <arch>.lx_list    # list_sort.c + шимы + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-sort bin/<arch>/lx-sort
void-store-import void-disk.img put result/bin/lx-argv bin/<arch>/lx-argv
void-store-import void-disk.img put result/bin/lx-list bin/<arch>/lx-list
```
Запуск в vsh: `run bin/lx-sort` / `run bin/lx-argv` / `run bin/lx-list` (чистые вычислялки, обе арх).
