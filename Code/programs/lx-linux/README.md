# lx-linux — неизменённый код ядра Linux на VOID (Вехи 55–67, dde_linux-конвейер)

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
- **Веха 62** — **кооперативный планировщик Lx_kit** (`lx_sched.h`): задача = отдельный стек +
  setjmp/longjmp, один поток (модель Genode dde_linux) → `lx-sched`. **Костяк рантайма** под
  jiffies/таймеры/wait_event/workqueue/kthread. Это НЕ порт `.c` — это наш рантайм.
- **Веха 63** — **jiffies + таймеры** (`linux/jiffies.h`, `linux/timer.h`): `jiffies`/`HZ`/
  `time_after`/`msecs_to_jiffies` + `timer_list`/`mod_timer`/`timer_delete` → `lx-timer`. Очередь
  таймеров в Lx_kit, idle-путь планировщика двигает время и стреляет; **`msleep` стал УСТУПАЮЩИМ**.
- **Веха 64** — **очереди ожидания + completion** (`linux/wait.h`, `linux/completion.h`):
  `wait_event`/`wake_up`/`wait_event_timeout` + `wait_for_completion`/`complete` → `lx-wait`. Задача
  ждёт события железа, уступая процессор; таймаут — через таймер Вехи 63.
- **Веха 65** — **рабочие очереди** (`linux/workqueue.h`): `schedule_work`/`schedule_delayed_work`/
  `flush_*`/`cancel_*` → `lx-work`. Каждую очередь крутит задача-воркер; delayed — через таймер.
  У e1000 6.18 watchdog именно на delayed_work.
- **Веха 66** — **driver-model + module_init** (`linux/device.h`, `linux/module.h`): `struct device`/
  `device_driver`/`bus_type`, `driver_register`/`device_register` (match по шине → `.probe`),
  `module_init`-редирект в `lx_module_init` → `lx-driver`. **Начало перехода к самому драйверу** (под PCI).
- **Веха 67** — **PCI** (`linux/pci.h`, `linux/ioport.h`): `pci_dev` встраивает `struct device`,
  `pci_driver` — `device_driver`, `pci_register_driver` крутит ту же связку match/probe Вехи 66, но
  match идёт по `id_table` (vendor/device); конфиг-пространство/BAR'ы + `pci_enable_device`/`set_master`/
  `select_bars`/`ioremap_bar` → `lx-pci`. Полигон — синтетический **8086:100E (e1000)**.

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
- `lx_sched.h` — **рантайм, не шим** (Веха 62): кооперативный планировщик Lx_kit. Задача =
  отдельный стек + `setjmp`/`longjmp`, один поток; `lx_task_create`/`lx_sched_run`/`lx_sched_yield`/
  `lx_task_block`/`lx_task_unblock`. Тела — в `lx_kit.c` (+ арх-вставка `arch_execute`: смена SP на
  свой стек для riscv64/x86_64). Сюда сядут jiffies/таймеры, wait_event/wake_up, workqueue, request_irq.
- `jiffies.h`, `timer.h` — инфраструктура времени (Веха 63): `jiffies`/`HZ`/`time_after`/
  `msecs_to_jiffies`; `timer_list`/`timer_setup`/`mod_timer`/`timer_delete`/`from_timer`. Тела в
  `lx_kit.c`: `jiffies` двигается по монотонному времени VOID, очередь таймеров, idle-путь
  планировщика стреляет выстрелившими (softirq-контекст). `msleep` переведён на уступающий сон.
- `wait.h`, `completion.h` — синхронизация (Веха 64): `wait_event`/`wait_event_timeout`/`wake_up`
  (очередь ждущих задач — записи на их стеках, wake переводит в готовые, каждая перепроверяет
  условие) + `struct completion` (`wait_for_completion`/`complete` поверх wait_event). Тела
  `__lx_wait`/`__lx_wake_up` — в `lx_kit.c`; completion — inline. Таймаут — через таймер Вехи 63.
- `workqueue.h` — отложенная работа (Веха 65): `work_struct`/`delayed_work`, `INIT_WORK`/
  `INIT_DELAYED_WORK`, `schedule_work`/`schedule_delayed_work`/`queue_work`, `flush_*`, `cancel_*`,
  `alloc_workqueue`/system_wq. Тела в `lx_kit.c`: очередь обслуживает задача-воркер (работа
  исполняется в контексте задачи — можно спать); delayed — через таймер; flush — через wait_event.
- `device.h`, `module.h` — driver-model + модуль (Веха 66): `struct device`/`device_driver`/
  `bus_type` + `driver_register`/`device_register` (match по шине → `.probe`, откат при отказе),
  `dev_*`-логи, `dev_get/set_drvdata`; `module_init(fn)` → `int lx_module_init(void)` (фикс-имя,
  один драйвер на бинарь), `MODULE_*`/`module_param` — в пустоту. Тела реестра в `lx_kit.c`.
- `pci.h`, `ioport.h` — шина PCI поверх driver-model (Веха 67): `pci_dev` (встраивает `device`,
  BAR'ы `resource[]`, конфиг `lx_config[]`)/`pci_driver` (встраивает `device_driver`, `id_table`);
  `PCI_DEVICE`/`PCI_ANY_ID`/`PCI_VENDOR_ID_INTEL`; `pci_register_driver` (match по `id_table` →
  мост в `.probe(pdev, id)`), `pci_enable_device`/`set_master`/`set_mwi`/`select_bars`/
  `request_selected_regions`/`ioremap_bar`/`pci_resource_*`, конфиг-чтение/запись (LE),
  `pci_save_state`/`set_power_state`/`enable_wake` (учётные). Тела в `lx_kit.c`. `struct resource` —
  из `ioport.h`. Реальное окно регистров даёт `ioremap` по MMIO-cap VOID.

Наш рантайм и харнессы:
- **`lx_kit.c`** — **Lx_kit-рантайм**: тела `kmalloc/…/kfree` + `kmemdup/kstrdup/kstrndup` над кучей
  newlib + `printk` + `udelay`/`mdelay`/`ndelay` (буси-ожидание по монотонному времени) + **кооперативный
  планировщик** (Веха 62: задачи/`arch_execute`/yield/block/unblock) + **jiffies/таймеры** (Веха 63:
  очередь `timer_list`, idle-путь стреляет, уступающий `msleep`) + **очереди ожидания** (Веха 64:
  `__lx_wait`/`__lx_wake_up`) + **рабочие очереди** (Веха 65: задача-воркер, delayed через таймер) +
  **driver-model** (Веха 66: реестр драйверов/устройств, match/probe) + **PCI** (Веха 67: `pci_bus_type`
  с match по `id_table`, мосты probe/remove, конфиг/BAR/enable/master). Растёт к request_irq/DMA и e1000.
- Харнессы: **`main.c`** — sort; **`main_argv.c`** — argv_split; **`main_list.c`** — list_sort;
  **`main_bits.c`** — bitops; **`main_err.c`** — err.h; **`main_io.c`** — io.h; **`main_delay.c`** — delay.h;
  **`main_sched.c`** — планировщик (yield + block/unblock); **`main_timer.c`** — jiffies/таймеры
  (уступающий msleep + таймеры по возрастанию expires); **`main_wait.c`** — wait_event/wake_up,
  completion, wait_event_timeout; **`main_work.c`** — workqueue (FIFO-работы, delayed, cancel);
  **`main_driver.c`** — driver-model (module_init → register → match → probe → remove);
  **`main_pci.c`** — PCI (синтетический 8086:100E → match по id_table → probe читает BAR и конфиг).

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
nix-build nix -A <arch>.lx_sched   # lx_sched.h/lx_kit.c (планировщик) + harness → VOID-ELF
nix-build nix -A <arch>.lx_timer   # jiffies.h/timer.h/lx_kit.c (таймеры) + harness → VOID-ELF
nix-build nix -A <arch>.lx_wait    # wait.h/completion.h/lx_kit.c (ожидание) + harness → VOID-ELF
nix-build nix -A <arch>.lx_work    # workqueue.h/lx_kit.c (рабочие очереди) + harness → VOID-ELF
nix-build nix -A <arch>.lx_driver  # device.h/module.h/lx_kit.c (driver-model) + harness → VOID-ELF
nix-build nix -A <arch>.lx_pci     # pci.h/ioport.h/lx_kit.c (шина PCI) + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-<имя> bin/<arch>/lx-<имя>   # для каждого
```
Запуск в vsh: `run bin/lx-sort` / `lx-argv` / `lx-list` / `lx-bits` / `lx-err` / `lx-io` / `lx-delay` /
`lx-sched` / `lx-timer` / `lx-wait` / `lx-work` / `lx-driver` / `lx-pci` (чистые вычислялки, обе арх).
