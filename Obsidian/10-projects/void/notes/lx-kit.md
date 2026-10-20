---
title: Веха 56 — Lx_kit: аллокатор ядра + header-поверхность, реальный argv_split.c работает
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 56 — Lx_kit: фундамент ядрового рантайма (аллокатор памяти)

Следующий шаг dde_linux-конвейера после [[lx-linux]] (Веха 55): растим шим до **аллокатора ядра**
и заводим **Lx_kit** — C-рантайм ядрового API поверх примитивов VOID. Доказываем не «алгоритмом
без состояния» (как `sort.c`), а первым портированным `.c`, который **честно аллоцирует память
ядровым `kmalloc`**: неизменённый `lib/argv_split.c` из Linux 6.18.7. Это фундамент под реальный
драйвер (e1000 и далее): всё в ядре стоит на `kmalloc`/`kfree`.

## Что портировано (verbatim) и что наше

- **`linux-src/argv_split.c`** — **НЕИЗМЕНЁННЫЙ** `lib/argv_split.c` из Linux **6.18.7** (GPL-2.0,
  SPDX на месте, 95 строк): режет строку по пробелам в argv-массив, выделяя память `kstrndup` +
  `kmalloc_array`, освобождая `kfree`. Ни строчки не тронуто.
- **`linux/{kernel,slab,gfp,string,ctype,printk}.h`** — НАШИ новые шим-заголовки (растим
  «lx_emul-заголовки» с 3 до 9 файлов):
  - `slab.h` — семейство аллокатора: `kmalloc/kzalloc/kcalloc/kmalloc_array/krealloc/kfree` +
    дубликаторы `kmemdup/kstrdup/kstrndup`, `KMALLOC_MAX_SIZE` (защита от переполнения `n*size`);
  - `gfp.h` — тип `gfp_t` + флаги `GFP_KERNEL/GFP_ATOMIC/…` (у нас **игнорируются**: куча единая,
    контекст один — нужны лишь чтобы код ядра компилировался/читался как в Linux);
  - `string.h` — `mem…/str…` из newlib + объявления `argv_split()/argv_free()` (в ядре они живут
    именно в `<linux/string.h>`);
  - `ctype.h` — `isspace` и семья, static inline по ASCII (в ядре — таблица `_ctype[]`);
  - `kernel.h` — «зонтик» идиом: `container_of`, `min/max/min3`, `ARRAY_SIZE`, `DIV_ROUND_UP`,
    `ALIGN/round_up/round_down`, `swap`;
  - `printk.h` — `printk()` + уровни `KERN_*` (пустышки) + обёртки `pr_info/pr_err/…`.
- **`lx_kit.c`** — НАШ **Lx_kit-рантайм**: тела аллокатора (над кучей newlib: `malloc/free` →
  `_sbrk` → один `SYS_MAP`) + `printk` (`vprintf` → stdout VOID). Первый C-файл рантайма kit;
  растёт к slab-кэшам/таймерам/workqueue/ioremap/DMA под реальный драйвер.
- **`main_argv.c`** — НАШ харнесс: зовёт реальный `argv_split()`, проверяет argc/токены/NULL-терминацию,
  печатает ядровым `printk`.

## Почему это шаг вперёд от Вехи 55

`sort.c` (Веха 55) — чистая вычислялка: ничего не аллоцировал, шима хватало из 3 заголовков.
`argv_split.c` **опирается на аллокатор ядра** — а значит на живой рантайм (`kmalloc` ↔ куча
процесса VOID). Заведён `Lx_kit` как отдельный C-рантайм: место, куда дальше сядут таймеры,
workqueue, ioremap/DMA (они уже есть в C-каркасе [[lx-emul-c]] — предстоит свести).

## Сборка/доставка/запуск (C-мир, как [[lx-linux]])

```
nix-build nix -A <arch>.lx_argv     # кросс-gcc: argv_split.c + шимы + lx_kit.c + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-argv bin/<arch>/lx-argv
run bin/lx-argv                     # в vsh; чистая вычислялка (аллокатор — куча процесса), обе арх
```
`nix/default.nix` — деривация `lx_argv` (`$CC … -I. -DCONFIG_64BIT -static main_argv.c
linux-src/argv_split.c lx_kit.c`). ELF на диске (`void-disk.img` в .gitignore).

Проверено (QEMU, обе арх): `run bin/lx-argv` →
`[lx-argv] Linux lib/argv_split.c (6.18.7, неизменённый) на VOID: argc=4 ['eth0', 'up', 'mtu', '1500']`
→ `[lx-argv] разбор верен -- аллокатор ядра (kmalloc) + строки РАБОТАЮТ на VOID`, код выхода 0.
`lib/sort.c` (Веха 55) пересобран против выросших общих заголовков — регрессии нет. Предупреждений
нет (`-Wall -Wextra` чисто на обеих арх). Ядро не менялось.

Грабля: в блок-комментарии нельзя писать `mem*/…` или `linux/*.h` — `*/` рвёт комментарий, `/*`
даёт `-Wcomment`; переформулировано словами.

## Что дальше (к драйверу e1000)

Аллокатор и базовая header-поверхность есть. Арка к реальному драйверу e1000 (полигон):
- **Sync/утиль ядра**: `err.h` (ERR_PTR/IS_ERR), `bitops.h`, `list.h` (container_of уже есть),
  `spinlock/mutex`, `jiffies`/`delay`, `io.h` — по мере файлов.
- **Lx_kit-рантайм** дорастить: workqueue/timer/completion/wait поверх нитей+futex (основа в
  [[lx-emul-c]]), свести с ioremap/DMA/`request_irq` того же C-каркаса.
- **driver-model + PCI** (Веха B) → **netdev-подмножество** (alloc_etherdev/netif/sk_buff/NAPI,
  Веха C) — чтобы скомпилировался `e1000.h` (тянет весь сетевой стек ядра).
- **сам драйвер** `drivers/net/ethernet/intel/e1000/*.c` (Веха D): `e1000_hw.c` (5630 строк, слой
  железа) → `e1000_main.c` (probe/tx/rx). Закроет «длинный хвост» железа X54C (Atheros/EHCI/wifi).

Лицензии: vendored-файлы Linux — под GPL-2.0 (свои SPDX); шимы/рантайм/harness — код проекта.

## Файлы

- `programs/lx-linux/linux-src/argv_split.c` — verbatim Linux 6.18.7 (GPL-2.0).
- `programs/lx-linux/linux/{kernel,slab,gfp,string,ctype,printk}.h` — новые шим-заголовки.
- `programs/lx-linux/lx_kit.c` — Lx_kit-рантайм (аллокатор + printk); `main_argv.c` — харнесс.
- `nix/default.nix` — деривация `lx_argv`.

## Связано
- [[lx-linux]] (Веха 55, sort.c — старт конвейера) · [[lx-emul-c]] (C-каркас: ioremap/DMA/IRQ/
  completion — предстоит свести с Lx_kit) · [[lx-emul]] (Rust-каркас) · [[nixpkgs-cross]] (C-мир) ·
  [[store-bridge]] (доставка) · [[userspace-drivers]] · [[irq]] · [[known-gaps]] · [[todo]]
