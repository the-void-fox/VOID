# lx-linux — неизменённый код ядра Linux на VOID (Вехи 55–61, dde_linux-конвейер)

Порт реальных `.c` из ядра Linux: неизменённый файл ядра компилируется против рукописных
шим-заголовков `linux/*.h` («lx_emul-заголовки») + C-рантайма `lx_kit.c` (Lx_kit) и работает на
VOID. Растёт по мере роста портируемого кода к настоящему драйверу (полигон — e1000).

- **Веха 55** — пилот конвейера: `lib/sort.c` (heapsort, ничего не аллоцирует) → `lx-sort`.
- **Веха 56** — фундамент **Lx_kit**: аллокатор ядра (`kmalloc`-семейство) + header-поверхность;
  первый `.c`, честно аллоцирующий память ядровым `kmalloc` — `lib/argv_split.c` → `lx-argv`.
- **Веха 57** — **`linux/list.h`**: двусвязный список ядра (костяк драйверов); реальный
  `lib/list_sort.c` (merge-сортировка списка) → `lx-list`.
- **Веха 58** — **`linux/bitops.h`**: битовые операции ядра; реальный `lib/hweight.c`
  (popcount) → `lx-bits`.
- **Веха 59** — **`linux/err.h`**: идиома «ошибка в указателе» (ERR_PTR/IS_ERR) → `lx-err`.
- **Веха 60** — **`linux/io.h`**: MMIO-аксессоры регистров (readl/writel) → `lx-io`.
- **Веха 61** — **`linux/delay.h`**: паузы тайминга железа (udelay/mdelay) → `lx-delay`.

## Что здесь

Vendored (**НЕИЗМЕНЁННЫЕ**, verbatim из Linux **6.18.7**, GPL-2.0, SPDX на месте):
- **`linux-src/sort.c`** + **`linux/sort.h`** — `lib/sort.c` и `include/linux/sort.h` (Веха 55).
- **`linux-src/argv_split.c`** — `lib/argv_split.c` (Веха 56).
- **`linux-src/list_sort.c`** + **`linux/list_sort.h`** — `lib/list_sort.c` и `include/linux/list_sort.h` (Веха 57).
- **`linux-src/hweight.c`** — `lib/hweight.c` (Веха 58).

Наши шим-заголовки `linux/*.h` + `asm/*.h` (lx_emul: дают ядровому коду ровно тот API, что он ждёт, под VOID):
- `types.h`, `export.h`, `sched.h` — минимум под sort.c (Веха 55).
- `kernel.h`, `slab.h`, `gfp.h`, `string.h`, `ctype.h`, `printk.h` — под argv_split.c (Веха 56):
  аллокатор/строки/классификация/печать/идиомы. Флаги `gfp_t` игнорируются (куча единая).
- `list.h`, `compiler.h`, `container_of.h` — под list_sort.c (Веха 57): двусвязный список +
  likely/unlikely; `container_of` вынесен в свой заголовок (как в ядре 6.x).
- `bitops.h`, `asm/types.h` — под hweight.c (Веха 58): битовые операции + `__u8..__u64`;
  `hweight*` маршрутизирован в реальный `hweight.c`. Оговорка: `set_bit` пока не атомарен.
- `err.h`, `errno.h` — инфраструктура (Веха 59): «ошибка в указателе» (ERR_PTR/IS_ERR) +
  проход к newlib errno. Проверяется харнессом (`main_err.c`) — в бою раскроется в драйвере.
- `io.h` — инфраструктура (Веха 60): MMIO-аксессоры `readl`/`writel`/… + `ioremap` (пока identity;
  настоящее окно — от lx_emul по MMIO-cap). Харнесс (`main_io.c`) — round-trip на буфере-регистрах.
- `delay.h` — инфраструктура (Веха 61): `udelay`/`mdelay`/`ndelay` — буси-паузы; тела в `lx_kit.c`
  (буси-ожидание по монотонному времени VOID). Харнесс (`main_delay.c`) замеряет реальную паузу.

Наш рантайм и харнессы:
- **`lx_kit.c`** — **Lx_kit-рантайм**: тела `kmalloc/…/kfree` + `kmemdup/kstrdup/kstrndup` над кучей
  newlib + `printk` + `udelay`/`mdelay`/`ndelay` (буси-ожидание по монотонному времени). Растёт к
  таймерам/workqueue/ioremap/DMA.
- Харнессы: **`main.c`** — sort; **`main_argv.c`** — argv_split; **`main_list.c`** — list_sort;
  **`main_bits.c`** — bitops; **`main_err.c`** — err.h; **`main_io.c`** — io.h; **`main_delay.c`** — delay.h.

Лицензии: vendored-файлы Linux остаются под GPL-2.0 (свои SPDX-заголовки); шимы, рантайм и харнессы —
код проекта. Хостинг Linux-драйверов по природе смешивает лицензии (портируемые части — GPL).

## Сборка (C-мир: nix-build → мост, как lx_e1000)

```
nix-build nix -A <arch>.lx_sort    # sort.c + шимы + harness → VOID-ELF (<arch> = x86_64 | riscv64)
nix-build nix -A <arch>.lx_argv    # argv_split.c + шимы + lx_kit.c + harness → VOID-ELF
nix-build nix -A <arch>.lx_list    # list_sort.c + шимы + harness → VOID-ELF
nix-build nix -A <arch>.lx_bits    # hweight.c + bitops.h/asm-types + lx_kit.c + harness → VOID-ELF
nix-build nix -A <arch>.lx_err     # err.h/errno.h + lx_kit.c + harness → VOID-ELF
nix-build nix -A <arch>.lx_io      # io.h + lx_kit.c + harness → VOID-ELF
nix-build nix -A <arch>.lx_delay   # delay.h + lx_kit.c + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-<имя> bin/<arch>/lx-<имя>   # для каждого
```
Запуск в vsh: `run bin/lx-sort` / `lx-argv` / `lx-list` / `lx-bits` / `lx-err` / `lx-io` / `lx-delay`
(чистые вычислялки, обе арх).
