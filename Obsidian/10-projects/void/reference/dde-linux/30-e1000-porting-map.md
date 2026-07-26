---
title: e1000 — карта порта (файлы, include-цепочка, что есть vs надо)
created: 2026-07-01
tags: [project/void, topic/dde-linux, topic/reference, topic/e1000]
status: active
---

# e1000 — карта порта

Полигон-драйвер: Intel PRO/1000 (`drivers/net/ethernet/intel/e1000/`), Linux **6.18.7**
(наш офлайн-тарбол, см. [[00-overview]]). Здесь — конкретный список файлов, include-цепочка
и приоритизация, что уже закрыто Вехами 55–61 и что предстоит. Рантайм — [[10-lx-kit-runtime]],
метод — [[20-linker-driven-workflow]].

## Файлы драйвера (что копируем verbatim, GPL-2.0)

| Файл | строк | роль |
|---|---|---|
| `e1000_main.c` | 5330 | точка входа модуля, netdev-ops, TX/RX, probe, NAPI, ISR |
| `e1000_hw.c` | 5630 | доступ к железу: сброс, PHY/MII, EEPROM, линк |
| `e1000_ethtool.c` | 1893 | ethtool-ops (можно отложить/заглушить сперва) |
| `e1000_param.c` | 727 | параметры модуля (можно упростить) |
| `e1000.h` | 351 | приватный заголовок драйвера — тянет всю поверхность (ниже) |
| `e1000_hw.h` | 3082 | регистры/структуры железа; тянет только `e1000_osdep.h` |
| `e1000_osdep.h` | — | **OS-adaptation слой** — тут наши `er32/ew32`→`readl/writel`, delay, типы |

Извлечение: `tar -xJf <тарбол> -O linux-6.18.7/drivers/net/ethernet/intel/e1000/<файл>`.

**`e1000_osdep.h`** — ключевая точка: это *санкционированный ядром* слой ОС-адаптации
(так и задуман). `er32(reg)`/`ew32(reg,val)` там сводятся к `readl/writel` по `hw->hw_addr`
(наш [[lx-io]], Веха 60). Минимально можно поправить именно его, не трогая логику.

## Include-цепочка `e1000.h` — приоритизация

### ✅ Уже есть (Вехи 55–61)
`linux/types.h` · `linux/errno.h` · `linux/string.h` · `linux/slab.h` · `linux/kernel.h` ·
`linux/list.h` · `linux/bitops.h` · `linux/delay.h` · `linux/io.h` (+ `asm/io.h`) ·
`linux/stddef.h` (тривиально) · плюс инфра `err.h`/`compiler.h`/`container_of.h`/`gfp.h`/
`ctype.h`/`printk.h`.

### 🟡 Малые/средние шимы (написать по мере линковки)
- `asm/byteorder.h` — `cpu_to_le32`/`le16_to_cpu`/… (LE, у нас обе арх LE — почти тождества +
  bswap для BE-полей). Нужен рано (e1000_hw.c).
- `linux/module.h` — `module_init/exit` редирект + пустые `MODULE_*` (см. [[20-linker-driven-workflow]]).
- `linux/timer.h` — `timer_list`/`mod_timer`/`del_timer` → на очередь таймеров рантайма ([[10-lx-kit-runtime]]).
- `linux/interrupt.h` — `request_irq`/`free_irq`/`tasklet`/`napi` → рантайм + `SYS_IRQ_WAIT`.
- `linux/ioport.h` — `struct resource`/`request_region` (в основном заглушки, у нас cap).
- `linux/reboot.h` · `linux/prefetch.h` · `linux/capability.h` — почти пусто/no-op.
- `linux/mii.h` · `linux/ethtool.h` — MII-хелперы + ethtool-ops (ethtool можно сперва заглушить).
- `asm/irq.h` — минимум.

### 🔴 Крупные подсистемы (главная работа)
- **`linux/pci.h`** — driver-model + PCI: `pci_driver`/`pci_device_id`/`pci_enable_device`/
  `pci_iomap`/`pci_set_master`/BAR'ы/`dma_set_mask`. Стыкуется с нашим PCI-обходом и MMIO/DMA-cap
  (Вехи 51–52, [[userspace-drivers]], [[irq]]). **Это следующий крупный блок после рантайма.**
- **`linux/netdevice.h`** — ядро сетевого стека: `net_device`/`netdev_ops`/`napi_struct`/
  `netif_*`/регистрация netdev. Большой, но нужен subset (см. ниже).
- **`linux/skbuff.h`** — `sk_buff`: alloc/free/`skb_put`/`skb_reserve`/фрагменты. Формат буфера
  на стыке с нашим `net-srv` (Веха 34, [[virtio-net]]).
- **`linux/etherdevice.h`** — `alloc_etherdev`/`eth_type_trans`/`is_valid_ether_addr` (поверх netdev/skb).
- **`linux/dma-mapping.h`** — `dma_alloc_coherent`/`dma_map_single`/`dma_unmap_*` → наш DMA-cap
  (Веха 51). Кольца TX/RX e1000 живут тут.
- **`linux/mm.h`** · `linux/pagemap.h` · `linux/vmalloc.h` — память/страницы (нужен узкий subset).

### 🟢 Протокольные заголовки — для TX-checksum-offload (можно отложить)
`linux/in.h` · `linux/ip.h` · `linux/ipv6.h` · `linux/tcp.h` · `linux/udp.h` · `linux/if_vlan.h` ·
`net/checksum.h` · `net/ip6_checksum.h` · `net/pkt_sched.h`. Нужны для разбора заголовков при
аппаратном офлоуде контрольных сумм/сегментации. **Сперва отключить офлоуд** (features=0) —
тогда бóльшая часть этого сводится к заглушкам/минимуму, и драйвер шлёт «как есть».

## Netdev-subset — что реально нужно от `netdevice.h`

Не весь стек, а то, что дёргает e1000: регистрация/дерегистрация `net_device`;
`netdev_ops` (`ndo_open/stop/start_xmit/…`); очереди (`netif_start/stop/wake_queue`,
`netif_carrier_on/off`); NAPI (`napi_schedule`/`napi_complete`/`netif_napi_add`, poll);
статистика. RX/TX замыкаем на наш `net-srv` вместо ядрового IP-стека (у Genode — `uplink.h`;
у нас — мост к virtio-net/ARP-стеку Вехи 34).

## Порядок вех (предложение)

1. **Рантайм-костяк** ([[10-lx-kit-runtime]]): задачи (setjmp/longjmp+стек), планировщик,
   jiffies+таймеры, `wait_event/wake_up`, `schedule_work`. → маленькие харнессы, обе арх.
2. **module/initcall/заглушки** ([[20-linker-driven-workflow]]): `module.h`, генератор стабов.
3. **PCI/driver-model** (`pci.h`) поверх нашего PCI-обхода + MMIO/DMA-cap.
4. **netdev-subset + skb + etherdevice + dma-mapping** — до момента, когда `e1000.h`
   компилируется, а `e1000_hw.c` линкуется.
5. **`e1000_hw.c`**: сброс/линк/EEPROM/PHY проходят (тут работают наши delay/io Вех 60–61).
6. **`e1000_main.c`**: probe находит устройство, `open`, кольца TX/RX через DMA-cap, ISR/NAPI.
7. **Мост наружу**: RX→`net-srv`, TX←`net-srv`; `ping` через портированный e1000.
8. Офлоуд/ethtool/param — по вкусу, после базового TX/RX.

Профит после e1000: тот же конвейер закрывает **Atheros NIC / EHCI / wifi** X54C —
меняется список `.c` и небольшой subset API, костяк ([[10-lx-kit-runtime]] +
[[20-linker-driven-workflow]]) переиспользуется.

## Связано
- [[00-overview]] · [[10-lx-kit-runtime]] · [[20-linker-driven-workflow]] · [[sources]]
- Наш e1000 (родной, не порт): Веха 49, [[e1000]]. Userspace-драйверы/IRQ: [[userspace-drivers]], [[irq]].
