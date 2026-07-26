---
title: Источники — dde_linux/lx_emul референс
created: 2026-07-01
tags: [project/void, topic/dde-linux, topic/reference]
status: active
---

# Источники

Откуда собрана эта папка (сбор — июль 2026). Genode — живой проект; при расхождении
верить исходникам `genodelabs/genode` (ветка master).

## Статьи / доки Genode

- **genode.org — Porting device drivers** (`genode.org/documentation/developer-resources/porting_device_drivers`)
  — общий конвейер: `lx_emul.h` через `CC_C_OPT += -include`, `dummies.cc` с макросом `DUMMY`
  для неопределённых ссылок, `#define module_init(fn) void module_##fn(void){fn();}`,
  Lx_kit даёт память/vmem/phys-lookup/IRQ/таймеры, порядок compile→link→init→probe→session.
- **Genode Release Notes 21.08** (`genode.org/documentation/release-notes/21.08`)
  — новая эпоха dde_linux: слои `lx_kit` (C++-планировщик), `lx_emul` (C-примитивы `lx_emul_*`),
  `genode_c_api` (тонкая обёртка сессий). Инструменты `create_dummies`→`generated_dummies.c`,
  `extract_initcall_order`→`initcall_order.h`. Принцип: **C++ никогда не включает заголовки Linux**.
  → распределено в [[00-overview]], [[10-lx-kit-runtime]], [[20-linker-driven-workflow]].
- **skalk — DDE-Linux experiments** (`genodians.org/skalk/2021-04-06-dde-linux-experiments`)
  — ранняя стадия «переиспользуем все заголовки ядра как есть» вместо ручных шимов; `target.mk`
  с флагами/`__KERNEL__`, сбор `.o`→vpath, `KBUILD_MODNAME` через Make-магию. Явно
  экспериментально; стабы/initcall — «в следующей статье».
- **nfeske — Pine fun: networking** (`genodians.org/nfeske/2021-09-03-pine-fun-networking`)
  — сетевой драйвер (STMMAC) под lx_emul: `initcall_order.h`, shadow `mm/slub.c`,
  `drivers/of/fdt.c`, deferred probe (`drivers/base/dd.c`, `-EPROBE_DEFER`), стык через
  `genode_c_api/uplink.h` + `lx_user.c`; тест через встроенный `net/ipv4/ipconfig.c` (DHCP).
  Модель: кооперативная задача, все нити свёрнуты в один поток. → [[10-lx-kit-runtime]], [[30-e1000-porting-map]].

## Исходники genodelabs/genode (raw, master)

- `repos/dde_linux/src/lib/lx_kit/scheduler.cc` — `_present_list`, `_execute()` (первая runnable,
  рестарт с головы ради приоритета), `unblock_irq_handler/time_handler`, «сигнал→повторный вход».
- `repos/dde_linux/src/lib/lx_kit/task.cc` — Task = отдельный стек (~32 КиБ) + `setjmp/longjmp`:
  `run()`/`schedule()`/`block_and_schedule()`, `arch_execute(stack, func, arg)`.
- Полезно ещё глянуть при реализации: `repos/dde_linux/src/lib/lx_emul/` (тела примитивов),
  `repos/dde_linux/src/include/lx_kit/`, `tool/dde_linux/{create_dummies,extract_initcall_order}`.
  → распределено в [[10-lx-kit-runtime]], [[20-linker-driven-workflow]].

## Исходник Linux (наш офлайн-тарбол)

- `/nix/store/yq0jp3zg48krjmiway1qxpny39vni8m7-linux-6.18.7.tar.xz`
- Извлечь файл: `tar -xJf <тарбол> -O linux-6.18.7/<путь>`
- e1000: `drivers/net/ethernet/intel/e1000/{e1000_main.c,e1000_hw.c,e1000_ethtool.c,e1000_param.c,
  e1000.h,e1000_hw.h,e1000_osdep.h,Makefile}` — размеры/цепочка в [[30-e1000-porting-map]].

## Связано
- [[00-overview]] · [[10-lx-kit-runtime]] · [[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
