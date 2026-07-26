---
title: Веха 58 — linux/bitops.h (битовые операции ядра), реальный hweight.c работает
created: 2026-07-01
tags: [project/void, topic/driver, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/lx-kit, topic/c]
status: done
---

# Веха 58 — битовые операции ядра (linux/bitops.h)

Третий кирпич kit после аллокатора ([[lx-kit]], Веха 56) и списка ([[lx-list]], Веха 57):
**битовые операции** — вездесущи в драйверах (флаги состояния, битовые карты очередей/векторов
прерываний, маски регистров, popcount). e1000 их использует напрямую. Даём `linux/bitops.h` и
доказываем неизменённым `lib/hweight.c` из Linux 6.18.7 (софтовый popcount ядра).

## Что портировано (verbatim) и что наше

- **`linux-src/hweight.c`** — **НЕИЗМЕНЁННЫЙ** `lib/hweight.c` из Linux **6.18.7** (GPL-2.0, SPDX на
  месте, 68 строк): `__sw_hweight8/16/32/64` — классический SWAR-popcount (число единичных бит).
  Ни строчки не тронуто.
- **`linux/bitops.h`** (наш шим) — привычный набор: `BIT`/`BIT_ULL`/`GENMASK`/`BITS_PER_LONG`/
  `BITS_TO_LONGS`/`DECLARE_BITMAP`; `set_bit`/`clear_bit`/`change_bit`/`test_bit` +
  `test_and_set_bit`/`test_and_clear_bit`; `find_first_bit`/`find_next_bit` + `for_each_set_bit`;
  `ffs`/`fls`/`__ffs`/`__fls`; `hweight8/16/32/64`/`hweight_long`. Софтовый popcount `hweight*`
  **маршрутизируется в реальный `hweight.c`** (`extern __sw_hweight*`).
- **`asm/types.h`** (наш шим) — `__u8..__u64`/`__s8..__s64` (в ядре из asm-generic/int-ll64.h);
  `hweight.c` включает `<asm/types.h>` напрямую. `-I.` находит и `linux/`, и `asm/`.
- **`main_bits.c`** (наш харнесс) — гоняет весь bitops.h: `set_bit` в 128-битную карту, `test_bit`,
  `test_and_clear_bit`, `for_each_set_bit` (обход [3 7 64 100]), `hweight32/64/16/8/long`,
  `BIT`/`GENMASK`/`fls`/`__ffs`. Печать — ядровым `printk`.

## Важная оговорка

`set_bit`/`clear_bit` тут **НЕ атомарны** (одно ядро, драйвер-харнесс однопоточный). Под
конкурентный IRQ-поток (нити lx_emul, [[irq]]/[[lx-emul-c]]) позже понадобятся настоящие атомики —
тогда допишем. Поиск бит — простой по-битовый (ядро оптимизирует по словам); нам довольно
корректности. `find_bit.c`/`bitmap.c` ядра тянут больше (swab/random/device/slab) — берём точечно.

## Сборка/доставка/запуск (C-мир, как [[lx-linux]])

```
nix-build nix -A <arch>.lx_bits     # hweight.c + bitops.h/asm-types.h + lx_kit.c + harness → ELF
void-store-import void-disk.img put result/bin/lx-bits bin/<arch>/lx-bits
run bin/lx-bits                     # в vsh; чистая вычислялка, обе арх
```
`nix/default.nix` — деривация `lx_bits` (`$CC … -I. -DCONFIG_64BIT -static main_bits.c
linux-src/hweight.c lx_kit.c`; `lx_kit.c` — ради `printk`). ELF на диске.

Проверено (QEMU, обе арх): `run bin/lx-bits` →
`[lx-bits] … биты [3 7 64 100], hweight64(~0)=64, hweight32(0xF0F0F0F0)=16`
→ `[lx-bits] битовые операции верны -- bitops ядра Linux РАБОТАЮТ на VOID`, код 0.
Предупреждений нет (`-Wall -Wextra`, обе арх). Ядро не менялось.

## Что дальше (к драйверу e1000)

Три столпа kit есть: аллокатор (56), список (57), биты (58). Дальше по арке:
- ещё утиль ядра: `err.h` (ERR_PTR/IS_ERR), `io.h` (MMIO поверх нашего ioremap), `delay.h`
  (udelay/mdelay), `jiffies`/времена, `atomic`, `spinlock`/`mutex` — по мере файлов;
- **Lx_kit-рантайм**: timer/workqueue/completion/wait поверх нитей+futex (основа в [[lx-emul-c]]),
  свести с ioremap/DMA/`request_irq`;
- **driver-model + PCI** → **netdev-подмножество** (alloc_etherdev/netif/sk_buff/NAPI), чтобы
  скомпилировался `e1000.h` (тянет весь сетевой стек ядра);
- **сам драйвер** `drivers/net/ethernet/intel/e1000/*.c` (e1000_hw.c → e1000_main.c). Закроет
  «длинный хвост» железа X54C (Atheros/EHCI/wifi).

Лицензии: vendored-файлы Linux — под GPL-2.0 (свои SPDX); шимы/харнесс — код проекта.

## Файлы

- `programs/lx-linux/linux-src/hweight.c` — verbatim Linux 6.18.7 (GPL-2.0).
- `programs/lx-linux/linux/bitops.h`, `asm/types.h` — новые шим-заголовки.
- `programs/lx-linux/main_bits.c` — харнесс; `nix/default.nix` — деривация `lx_bits`.

## Связано
- [[lx-list]] (Веха 57, список) · [[lx-kit]] (Веха 56, аллокатор) · [[lx-linux]] (Веха 55, старт) ·
  [[lx-emul-c]] (C-каркас: IRQ-поток — под атомики позже) · [[store-bridge]] · [[known-gaps]] · [[todo]]
