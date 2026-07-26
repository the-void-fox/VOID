---
title: Веха 55 — первый неизменённый .c из ядра Linux работает на VOID
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/c]
status: done
---

# Веха 55 — реальный код ядра Linux на VOID (старт dde_linux-конвейера)

Начало порта **реальных `.c` из ядра Linux** (после каркасов lx_emul: Rust [[lx-emul]], C [[lx-emul-c]]).
Конечная цель — хостить неизменённые Linux-драйверы (Genode `dde_linux`); подход: реальный `.c`
компилируется против шим-заголовков `linux/*.h`, подставляющих ему ядровый API поверх VOID. Эта
веха поднимает сам **конвейер** на маленьком самодостаточном файле — `lib/sort.c` (heapsort) —
чтобы доказать: неизменённый исходник ядра Linux собирается и работает на VOID. Хардкор драйверов
(Lx_kit, сотни функций) впереди; здесь — фундамент header-shim, переиспользуемый дальше.

## Что портировано (verbatim) и что наше

- **`linux-src/sort.c`** — **НЕИЗМЕНЁННЫЙ** `lib/sort.c` из Linux **6.18.7** (357 строк, GPL-2.0,
  SPDX на месте): heapsort с обобщённым swap (32/64-бит/побайтно) и колбэками-указателями.
- **`linux/sort.h`** — НЕИЗМЕНЁННЫЙ `include/linux/sort.h` (в нём же макрос `cmp_int` ядра).
- **`linux/{types,export,sched}.h`** — НАШИ шим-заголовки (первый кусок «lx_emul-заголовков»):
  `types.h` — `u8..u64` + typedef'ы колбэков (`cmp_func_t`/`swap_r_func_t`…) + `__always_inline`/
  `__attribute_const__`; `export.h` — `EXPORT_SYMBOL` пустышкой (всё статически в один ELF);
  `sched.h` — `cond_resched()` пустышкой (у нас вытеснение по таймеру; Lx_kit — позже).
- **`main.c`** — НАШ харнесс: массив + колбэк `int_cmp` (внутри — ядровый макрос `cmp_int`), зов
  реального `sort()`, проверка порядка, печать.

Источник Linux (6.18.7) достаётся из nix-стора **оффлайн** (тарболл уже реализован; сети нет).
`-DCONFIG_64BIT` на компиляции — обе арх 64-битные (64-битный swap). Ни строчки sort.c не тронуто.

## Сборка/доставка/запуск (C-мир, как lx_e1000)

```
nix-build nix -A <arch>.lx_sort               # кросс-gcc: sort.c + шимы + harness → VOID-ELF
void-store-import void-disk.img put result/bin/lx-sort bin/<arch>/lx-sort
run bin/lx-sort                                # в vsh; чистая вычислялка, обе арх
```
`nix/default.nix` — деривация `lx_sort` (`$CC -B… -specs=void.specs -I. -DCONFIG_64BIT -static
main.c linux-src/sort.c`). ELF на диске (`void-disk.img` в .gitignore), воспроизводится этими
командами. Грабля: имя колбэка нельзя `cmp_int` — столкнётся с макросом из `linux/sort.h`.

Проверено (QEMU, обе арх): `run bin/lx-sort` →
`[lx-sort] Linux lib/sort.c (6.18.7, неизменённый) на VOID: 0 1 2 3 4 5 7 7 13 42 88 99 100 271 815`
→ `[lx-sort] порядок верен -- код ядра Linux РАБОТАЕТ на VOID`. Ядро не менялось; предупреждений нет.

## Что дальше (к драйверу)

Конвейер «реальный .c ядра → VOID» работает. Рост к драйверу — постепенный, по мере растущей
header-поверхности:
- больше `linux/*.h` шимов (bitops, kernel.h, string, err.h, slab.h…) под следующие файлы;
- **`Lx_kit`**: эмуляция scheduler/workqueue/timer/completion — Linux-драйвер ждёт вытесняющий
  контекст и отложенную работу (у нас есть нити+futex из lx_emul C — это основа);
- driver-model/pci/netdev-структуры, `request_irq`/`ioremap`/DMA уже есть в шиме (C lx_emul);
- потом реальный драйвер подсистемы (NIC/USB) — закроет Atheros/EHCI/wifi X54C.

Лицензии: vendored-файлы Linux — под GPL-2.0 (свои SPDX); шимы/harness — код проекта. Хостинг
Linux-драйверов по природе смешивает лицензии (портируемые части остаются GPL).

## Файлы

- `programs/lx-linux/linux-src/sort.c`, `linux/sort.h` — verbatim Linux 6.18.7 (GPL-2.0).
- `programs/lx-linux/linux/{types,export,sched}.h` — шим-заголовки (lx_emul).
- `programs/lx-linux/main.c` — харнесс; `README.md` — что vendored/наше.
- `nix/default.nix` — деривация `lx_sort`.

## Связано
- [[lx-emul]] (Rust-каркас) · [[lx-emul-c]] (C-каркас, тот же тулчейн) · [[nixpkgs-cross]]
  (C-мир) · [[store-bridge]] (доставка) · [[known-gaps]] · [[todo]]
