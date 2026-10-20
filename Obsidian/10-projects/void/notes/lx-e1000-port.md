---
title: Веха 68 — сам драйвер e1000 компилируется, линкуется и исполняется на VOID
created: 2026-07-01
tags: [project/void, topic/driver, topic/e1000, topic/linux-drivers, topic/lx-emul, topic/dde-linux, topic/netdev, topic/c]
status: done
---

# Веха 68 — неизменённый e1000: компилируется, линкуется, исполняется

Все опоры были готовы (Lx_kit-примитивы + driver-model + PCI, Вехи 56–67). Эта веха — **сам
драйвер**: неизменённые `e1000_hw.c`/`e1000_main.c`/`e1000_param.c` (Linux 6.18.7, verbatim, GPL-2.0)
теперь **компилируются, линкуются и исполняются** на VOID против рукописных шимов + Lx_kit. Метод —
компиляторно-/линкер-ориентированный ([[20-linker-driven-workflow]]): компилировать реальный `.c`,
добавлять РОВНО то, что требует ошибка, гасить неопределённые символы. Кросс-компилятор
(riscv64/x86_64, newlib, без Linux-заголовков) — оракул: `<asm/byteorder.h>` и прочее резолвятся
только в НАШИ шимы, а не в системные.

## Что наше

- **netdev-подмножество** (чтобы скомпилировался `e1000.h` — он тянет весь сетевой стек): ~25 новых
  шимов. Крупные: `netdevice.h` (`net_device`/`net_device_ops` с ndo_*/`napi_struct`/`netif_*`/фичи
  `NETIF_F_*`/`netif_msg_*`+`netif_err/info/...`), `skbuff.h` (`sk_buff` с линейкой+фрагментами,
  `skb_put`/`skb_reserve`/GSO), `etherdevice.h` (`alloc_etherdev`/`eth_type_trans`/MAC-хелперы),
  `dma-mapping.h` (`dma_alloc_coherent`/`dma_map_*`), `ethtool.h`/`mii.h`, `interrupt.h`
  (`request_irq`/`irqreturn_t`), `atomic.h`/`spinlock.h`/`mutex.h` (в один-поток-модели почти no-op).
  Мелкие: `mm.h`/`vmalloc.h`/`pagemap.h`, `if_vlan.h`/`ip.h`/`ipv6.h`/`tcp.h`/`udp.h`/`in.h` +
  `net/checksum.h`/`net/ip6_checksum.h`, `bitfield.h`/`prefetch.h`/`capability.h`/`reboot.h`/`stddef.h`,
  `asm/byteorder.h`/`asm/io.h`/`asm/irq.h`. Дорощены `types.h` (64-битные = `long long`, как ядро →
  `%ll` в vendored-коде совпадает; `__leNN`/`dma_addr_t`), `kernel.h` (`min_t`/`WARN_ON`/`print_hex_dump`/
  `system_state`), `compiler.h` (`fallthrough`/секц-атрибуты `__read_mostly`/…), `bitops.h` (`ffs`/`fls`
  берём из newlib `<strings.h>` — та же семантика, снят конфликт), `module.h` (`module_param_array_named`/
  `__MODULE_STRING`), `device.h` (`dev_pm_ops`/`DEFINE_SIMPLE_DEV_PM_OPS`).
- **`lx_net.c`** — сетевой рантайм/заглушки («generated_dummies», приём Genode): ТЕЛА ~77 символов
  netdev/skb/dma/napi/irq/страниц. Настоящие: `alloc_etherdev`/`free_netdev`, skb-линейка
  (`skb_put`/`reserve`/`trim`, аллокация буфера), `dma_alloc_coherent`/`dma_map_*` (куча, phys==dev-адрес),
  страницы, `strscpy`. No-op под пути, что оживут дальше: `register_netdev`/`netif_*`/`napi_*`/
  `request_irq`/`e1000_set_ethtool_ops`.
- **`main_e1000.c`** — харнесс: линкует vendored `e1000_hw.c`/`e1000_main.c`/`e1000_param.c` + `lx_kit.c`
  + `lx_net.c`, зовёт ЧИСТУЮ логику `e1000_hw.c` — `e1000_set_mac_type` (device 0x100E → e1000_82540) и
  `e1000_set_media_type` (читает регистр STATUS через OS-adaptation `er32`→`readl` по синтетическому окну).

## Сборка/запуск

`nix-build nix -A <arch>.lx_e1000_port` → мост `put … bin/<arch>/lx-e1000` → `run bin/lx-e1000`.
Vendored-файлы собираются с ядровыми флагами (`-Wno-unused-parameter`/`-Wno-pointer-sign`); наши
файлы (`lx_net.c`/`main_e1000.c`) — `-Wall -Wextra -Wcomment` чисто.

Проверено (QEMU, **обе арх идентично**): `e1000_set_mac_type(device=0x100e) → ret=0, mac_type=5`
(=e1000_82540); `e1000_set_media_type → media_type=0` (copper; читал STATUS через er32→readl);
`Результат: mac_type=5 media=0 — OK`. Весь драйвер: **0 ошибок / 0 предупреждений** на обеих
кросс-арках; 13 прежних `lx_*` вех пересобраны на обеих арх (регрессий нет, в т.ч. после смены
`u64` на `long long`); ядро VOID не менялось. Грабля (повтор): `linux/*.h` в комментарии даёт
`/*` → `-Wcomment`; переформулировал.

## Что дальше — оживить драйвер (probe/TX/RX)

Драйвер собран и исполняется, но пока лишь чистая логика. Дальше по [[30-e1000-porting-map]]:
- **probe против настоящего QEMU-e1000**: спавнить `lx-e1000` как userspace-драйвер, дать ему
  MMIO/DMA/IRQ-cap (Вехи 51–52, [[userspace-drivers]], [[irq]]); `pci_ioremap_bar` свести с
  `SYS_MMIO_MAP`, `dma_alloc_coherent` — с DMA-cap, `request_irq` — с задачей на `SYS_IRQ_WAIT`.
  Тогда e1000_probe читает MAC из EEPROM, сбрасывает чип, поднимает линк.
- **open + кольца TX/RX + ISR/NAPI**, потом мост RX→`net-srv` / TX←`net-srv` (Веха 34,
  [[virtio-net]]) → `ping` через ПОРТИРОВАННЫЙ e1000. Затем ethtool/offload по вкусу.
- Тот же конвейер закроет **Atheros / EHCI / wifi** X54C (меняется список `.c` + узкий subset API).

## Связано
- [[lx-pci]] (Веха 67) · [[lx-driver]] (Веха 66) · [[lx-kit]] · [[lx-emul-c]] · [[e1000]] (родной драйвер, Веха 49) ·
  [[userspace-drivers]] · [[irq]] · [[virtio-net]] · [[known-gaps]] · [[todo]]
- Референс: reference/dde-linux/[[20-linker-driven-workflow]] · [[30-e1000-porting-map]]
