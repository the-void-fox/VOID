---
title: Известные пробелы и техдолг VOID (живой список)
created: 2026-07-01
tags: [project/void, topic/roadmap, topic/techdebt]
status: active
---

# Известные пробелы и техдолг

Честная инвентаризация «недоделанного» на конец Вехи 51 (папки, редактор строки, освобождение фреймов, AHCI-диск, установка на диск, e1000-сеть, USB-клавиатура, userspace-драйверы взяты).
Разделяю **по дизайну** (осознанно так) и **техдолг/недоделки** (надо чинить). Обновлять по мере закрытия.

## Файловая система

- **У ядра ФС нет вообще — ЭТО ДИЗАЙН** ([[0002-persistent-content-addressed-capability-core]]):
  есть контент-адресуемый store (объекты по хэшу + корни + capability). «Файлы/папки» —
  это персоналия ([[posix-personality]]), не ядро.
- ~~posixfs ПЛОСКАЯ~~ → **ИЕРАРХИЯ добавлена (Веха 44, [[folders]])**: каталоги/пути/`cwd`,
  `cd`/`pwd`/`mkdir`/`rm`, вложенность, персистентно — файлы под корнями `f<path>`, каталоги
  под `d<path>` (GC-safe), без правок ядра. Осталось: прав (uid/gid/mode) и времён нет,
  `rm -r` нет (только пустой каталог), файл ≤128 КиБ, 16 открытых слотов, std-`create_dir` нет.

## Реальное железо (Вехи 41–47 — грузимся до vsh, диск на SATA есть)

- ~~Нет драйверов диска (AHCI/SATA)~~ → **AHCI есть (Веха 47, [[ahci]])**: store читает/пишет
  на реальный SATA-диск (проверено: запись пережила ребут). Опрос, одна команда, LBA48,
  BIOS в режиме AHCI. Осталось: NVMe, NCQ/прерывания.
- ~~Загрузка только с USB~~ → **установка на диск есть (Веха 48, [[install]])**: vsh `install`
  ставит VOID на SATA (GRUB+ядро в p1 FAT, store в p2 `0x9f`), дальше грузится с диска — проверено
  end-to-end в QEMU. Осторожно: **`install` стирает весь диск** (MBR с сектора 0). Осталось: GPT/UEFI,
  выбор диска, мульти-диск, обновление на месте (сейчас только полное стирание), NVMe-цель.
- ~~Нет реального NIC~~ → **e1000 есть (Веха 49, [[e1000]])**: драйвер Intel PRO/1000, ping отвечает
  (проверено в QEMU). НО у X54C проводной порт — Atheros/Realtek, не e1000 → на нём не поднимется
  (нужен драйвер того чипа). rtl8139/Atheros/wifi — отдельные драйверы.
- ~~USB-стека нет~~ → **xHCI + HID-клавиатура есть (Веха 50, [[usb]])**: свой USB-стек (контроллер
  → перечисление → HID boot keyboard), клавиатура на xHCI работает (проверено в QEMU). НО X54C =
  Sandy Bridge → **EHCI**, не xHCI → на нём этот драйвер USB не поднимет (там пока BIOS-legacy в
  `ps2.rs`). Осталось: EHCI, USB-мышь/флешки (MSC), хабы, прерывания вместо опроса.
- Нет **RTC** → часы фиктивные (`SystemTime` от выдуманной базы).
- Таймерный квант откалиброван под QEMU (~1 ГГц); на железе преемпшн-интервал «не тот».

## Память

- ~~Аллокатор фреймов — bump БЕЗ освобождения~~ → **освобождение есть (Веха 46, [[frame-freeing]])**:
  фреймы завершившихся процессов (страницы + таблицы + корень) возвращаются в список свободных и
  переиспользуются; проверено — пик не растёт при повторных запусках. Осталось: Vec `procs` (слоты
  Finished) не переиспользуется — это КУЧА, не фреймы; арена кучи ядра (16 МиБ) и кольца virtio не
  освобождаются осознанно.
- x86: операционная RAM зажата **256 МиБ** (чтобы не мапить гигабайты 4-КиБ страницами).
- Аллокатор фреймов **не уважает карту памяти** (bump поверх низкой RAM).

## Многозадачность / процессы

- **Один CPU, без SMP** — планировщик/локи/IPI однопроцессорные. Крупная переделка.
- Нет **пайпов и перенаправления** между процессами (`a | b`); `> file` только внутри posixfs.
- Нет фонового запуска / job control; нет настоящего fork (только exec-по-хэшу с ожиданием).
- Сигналы linux-abi игнорируются. Checkpoint — только однонитевой.

## Оболочка vsh

- ~~Нет редактирования строки~~ → **есть (Веха 45, [[tty]])**: курсор, вставка/удаление в позиции,
  `←`/`→`/`Home`/`End`/`Delete`, история `↑`/`↓` (PS/2-стрелки + serial, единый ANSI). Осталось:
  автодополнения нет, скроллбэка нет, строки шире экрана редактируются неидеально.
- Команда ≤128 байт.

## Бэкенды пакетов (все три «взяты», но минимальны)

- **linux-abi**: работают applet'ы в stdout (`echo`, `uname`); файловые (`cat FILE`, `ls`, `sh`)
  нет — нет моста linux→posixfs. Нет `clone`/`futex`, fork/exec.
- **wasi**: только hello (6 импортов). Нет доступа к файлам (preopen'ы), почти нет clock/random.

## Сеть

- Только **ARP + IPv4 + ICMP echo** (`ping`). Нет TCP/UDP/DNS.

## Прочее

- uutils — 8 утилит; `sort` отложен (rayon/getrandom).
- Безопасность capability **не аудирована**.

## Приоритет «на ощущение живой системы» (обсуждено с владельцем)

1. ✅ **Папки в posixfs** + `cd`/`pwd`/`mkdir` (Веха 44, [[folders]]) — сделано.
2. ✅ **Редактирование строки в vsh** (стрелки+история, Веха 45, [[tty]]) — сделано.
3. ✅ **Освобождение фреймов** (Веха 46, [[frame-freeing]]) — сделано, память не течёт.
4. Крупное: драйверы реального железа — покрывают ~80% PC своими силами:
   - ✅ **AHCI-диск** (Веха 47, [[ahci]]) — персистентность на SATA-железе.
   - ✅ **Установка на диск** (Веха 48, [[install]]) — vsh `install`, загрузка с диска без USB.
   - ✅ **NIC e1000** (Веха 49, [[e1000]]) — Intel PRO/1000, ping отвечает (QEMU + Intel-железо).
   - ✅ **USB xHCI + HID-клавиатура** (Веха 50, [[usb]]) — свой USB-стек, клава на xHCI работает.
   - **Хостить Linux-драйверы** (Genode-стиль) — для «длинного хвоста» железа X54C: Atheros NIC,
     EHCI, wifi, GPU. Идёт слоями:
     - ✅ **Фундамент: userspace-драйверы** — закрыт целиком: MMIO+DMA по cap (Веха 51,
       [[userspace-drivers]]) + доставка прерываний в userspace (Веха 52, [[irq]]) — e1000
       в userspace, IRQ по capability (`SYS_IRQ_WAIT`, IOAPIC level-low + oneshot-маска).
     - ✅ **Linux-API шим `lx_emul`** — каркас на Rust (Веха 53, [[lx-emul]]) и C (Веха 54,
       [[lx-emul-c]]): ioremap/kmalloc/dma_alloc_coherent/request_irq/driver-model, e1000 как
       Linux-стилевой драйвер (Rust `bin/lx_e1000` + C `lx_e1000_c`).
     - ✅ **Первый неизменённый `.c` из ядра Linux работает на VOID** (Веха 55, [[lx-linux]]):
       `lib/sort.c` 6.18.7 verbatim против рукописных шим-заголовков `linux/*.h` (начало
       dde_linux-конвейера), обе арх.
     - ✅ **Lx_kit: аллокатор ядра + header-поверхность** (Веха 56, [[lx-kit]]): заведён
       C-рантайм `lx_kit.c` (семейство `kmalloc`/`kfree` над кучей + `printk`); шимы выросли до
       `kernel/slab/gfp/string/ctype/printk.h`. Доказано неизменённым `lib/argv_split.c` 6.18.7 —
       первый портированный `.c`, честно аллоцирующий память ядровым `kmalloc`, обе арх.
     - ✅ **linux/list.h: двусвязный список ядра** (Веха 57, [[lx-list]]): костяк ядра/драйвера
       (очереди netdev, списки буферов). Шим `list.h` (LIST_HEAD/list_add_tail/list_for_each_entry)
       + `compiler.h` (likely/unlikely) + вынесенный `container_of.h`. Доказано неизменённым
       `lib/list_sort.c` 6.18.7 (устойчивая merge-сортировка списка) — порядок+целостность кольца, обе арх.
     - ✅ **linux/bitops.h: битовые операции ядра** (Веха 58, [[lx-bits]]): вездесущи в драйверах
       (флаги, битовые карты, маски регистров). Шим `bitops.h` (BIT/GENMASK/set_bit/test_bit/
       for_each_set_bit/ffs/fls/hweight) + `asm/types.h`; `hweight*` маршрутизирован в реальный
       `lib/hweight.c` 6.18.7 (софтовый popcount). Обе арх. Оговорка: set_bit пока не атомарен (под
       IRQ-поток нужны атомики).
     - ✅ **linux/err.h: «ошибка в указателе»** (Веха 59, [[lx-err]]): `ERR_PTR`/`PTR_ERR`/`IS_ERR`/
       … — идиома возврата ресурса-или-ошибки без out-параметра (драйверы юзают повсюду) + тонкий `errno.h`.
     - ✅ **linux/io.h: MMIO-аксессоры** (Веха 60, [[lx-io]]): `readl`/`writel`/… — доступ к
       регистрам устройства (e1000 весь на них через `er32`/`ew32`); `_relaxed`/`ioread`/`iowrite`;
       `ioremap` пока identity (настоящее окно — от lx_emul по MMIO-cap, сведём). Проверено харнессом
       на буфере-регистрах, обе арх.
     - ✅ **linux/delay.h: паузы тайминга** (Веха 61, [[lx-delay]]): `udelay`/`mdelay`/`ndelay` —
       буси-паузы (у e1000 сброс/линк/EEPROM — сплошь паузы); тела в Lx_kit — буси-ожидание по
       монотонному времени VOID (`gettimeofday`, 1–100 нс/тик). Харнесс замеряет, что пауза РЕАЛЬНО
       прошла, обе арх. **Трио инфраструктуры err/io/delay завершено.**
     - ✅ **Lx_kit-рантайм: кооперативный планировщик** (Веха 62, [[lx-sched]]): костяк рантайма
       (не шим) — задача = отдельный стек + `setjmp/longjmp`, ОДИН поток (модель Genode dde_linux;
       арх-вставка `arch_execute` для riscv64/x86_64). `lx_task_create`/`lx_sched_run`/`yield`/
       `block`/`unblock`. Следствие: внутри Linux-кода нет гонок, spinlock/mutex/атомики почти no-op.
       Харнесс: round-robin по yield + ping/pong по block/unblock (`pp_seq=121212`), отдельные стеки
       доказаны, обе арх. Референс подхода собран в `reference/dde-linux/`.
     - ✅ **jiffies + таймеры** (Веха 63, [[lx-timer]]): `linux/jiffies.h` (`jiffies`/`HZ=100`/
       `time_after`/`msecs_to_jiffies`) + `linux/timer.h` (`timer_list`/`mod_timer`/`timer_delete`/
       `from_timer`). Очередь таймеров в Lx_kit, idle-путь планировщика двигает время по монотонным
       часам VOID и стреляет выстрелившими (softirq-контекст); **`msleep` стал уступающим**. Харнесс:
       сони чередуются, таймеры `2 3 1`, обе арх. **Оговорки:** (а) jiffies двигается в точках
       планирования/задержки, не непрерывно — буси-цикл по `jiffies` без `msleep`/`udelay`/`cpu_relax`
       время не увидит; (б) idle-ожидание таймера — буси по реальному времени (жжёт хост-CPU, пока
       все задачи спят; настоящий сон ядра VOID — позже).
     - ✅ **Очереди ожидания + completion** (Веха 64, [[lx-wait]]): `linux/wait.h`
       (`wait_event`/`wait_event_timeout`/`wake_up`) + `linux/completion.h` (`wait_for_completion`/
       `complete`) — чем драйвер ждёт события железа (сброс/линк/DMA), уступая процессор. Поверх
       block/unblock: запись ждущего на стеке задачи, wake_up переводит в готовые (перепроверят
       условие сами — нет потерянных пробуждений в один-поток-модели); таймаут — через таймер Вехи 63.
       Харнесс: producer/consumer, completion, таймаут (истечение→0, пробуждение→остаток jiffies),
       обе арх.
     - ✅ **Рабочие очереди** (Веха 65, [[lx-work]]): `linux/workqueue.h` — `schedule_work`/
       `schedule_delayed_work`/`queue_work`/`flush_*`/`cancel_*`/`alloc_workqueue`/system_wq. Каждую
       очередь крутит задача-воркер (работа исполняется в контексте задачи — можно спать); delayed —
       через таймер (Веха 63); flush — через wait_event (Веха 64). У e1000 6.18 watchdog именно на
       delayed_work. Харнесс: FIFO-работы, delayed ~30 мс, cancel до срабатывания (не выполнилась),
       обе арх. Оговорка: воркер — демон, при остановке планировщика остаётся заблокированным (стек
       не реапится; для реального драйвера норма). **Рантайм-примитивы Lx_kit готовы** (память/
       список/биты + err/io/delay + планировщик/таймеры/ожидание/workqueue).
     - ✅ **driver-model + module_init** (Веха 66, [[lx-driver]]): `linux/device.h` (`struct device`/
       `device_driver`/`bus_type`, `driver_register`/`device_register` → match по шине → `.probe`,
       откат при отказе; `dev_*`-логи, drvdata) + `linux/module.h` (`module_init`→фикс-имя
       `lx_module_init`, `MODULE_*`/`module_param` — в пустоту). Упрощённый `drivers/base/dd.c`.
       Харнесс: фейк-шина/драйвер/устройство проходят module_init→register→match→probe→remove
       (`probe=1 remove=1 drvdata=0xabcd`), обе арх. **Начало перехода к самому драйверу** (метод —
       линкер-ориентированный).
     - ✅ **Шина PCI** (Веха 67, [[lx-pci]]): `linux/pci.h` (`pci_dev` встраивает `struct device`,
       `pci_driver` — `device_driver`; `id_table`/`PCI_DEVICE`/`PCI_VENDOR_ID_INTEL`; BAR'ы
       `resource[]`/конфиг `lx_config[]`) + `linux/ioport.h` (`struct resource`/`IORESOURCE_*`).
       `pci_register_driver` крутит ту же связку match/probe Вехи 66, но match идёт по `id_table`
       (vendor/device); `pci_enable_device`/`set_master`/`select_bars`/`request_regions`/`ioremap_bar`/
       `pci_resource_*`/конфиг-чтение-запись — тела в `lx_kit.c`. Харнесс: синтетический **8086:100E**
       (e1000) → match по id_table → probe «как e1000» читает BAR (`readl`=0xe1000ba5) и конфиг
       (`vendor=8086 device=100e`, `COMMAND=0x0007`) → remove; обе арх. **Все опоры под драйвер
       готовы** (kit + driver-model + PCI).
     - ✅ **Сам драйвер e1000 компилируется/линкуется/исполняется** (Веха 68, [[lx-e1000-port]]):
       неизменённые `e1000_hw.c`/`e1000_main.c`/`e1000_param.c` (6.18.7, verbatim, GPL-2.0) собраны
       против **netdev-подмножества** шимов (~25 новых: `netdevice.h`/`skbuff.h`/`etherdevice.h`/
       `dma-mapping.h`/`ethtool.h`/`mii.h`/`interrupt.h`/`atomic.h`/… + дорощены types/kernel/compiler/
       bitops) + рантайма `lx_kit.c` + сетевых заглушек `lx_net.c` («generated_dummies»). 0 ошибок/
       0 предупреждений обе арх. Харнесс `main_e1000.c` зовёт ЧИСТУЮ логику `e1000_hw.c`: `set_mac_type`
       (0x100E→e1000_82540), `set_media_type` (читает STATUS через `er32`→`readl`) → `mac_type=5 media=0
       OK`, обе арх идентично. **Vendored e1000-код исполняется на VOID.**
     - ✅ **Портированный e1000 читает НАСТОЯЩИЙ QEMU-e1000 через MMIO-cap** (Веха 69, [[lx-e1000-hw]]):
       тот же неизменённый `e1000_hw.c` спавнится init'ом как userspace-драйвер (путь Вех 51–54), маппит
       BAR0 по MMIO-cap (`vsys_mmio_map`, start_cap 0) в `hw->hw_addr` — и через `er32`/`ew32` сбрасывает
       РЕАЛЬНУЮ карту, читает STATUS, а MAC — из EEPROM реального e1000 (QEMU эмулирует microwire/PHY сам).
       Bring-up идёт как задача Lx_kit (vendored `msleep` уступает). `syscall.h` теперь ставится в void-libc;
       `init.rs` предпочитает `lx-e1000-hw`. Проверено (x86, `-device e1000,mac=…99`): `STATUS=0x80080783
       link UP`, `reset_hw`, `MAC=52:54:00:12:34:99` (совпал с заданным!), `speed=1000 duplex=full — OK`.
       Регрессии чисты (x86 без e1000 — `ping` ок; riscv — не спавнится). **Портированный драйвер трогает
       реальное железо.**
     - ✅ **Портированный e1000 ПЕРЕДАЁТ кадр (DMA-TX)** (Веха 70, [[lx-e1000-tx]]): `dma_alloc_coherent`
       в `lx_net.c` сведён с DMA-cap (`vsys_dma_alloc`, start_cap 1; guard `-DLX_HAVE_SYSCALL`). Драйвер
       строит TX-кольцо дескрипторов vendored `e1000_setup_all_tx_resources` на РЕАЛЬНОМ DMA, конфигурирует
       TX-движок (TDBAL/TDLEN/TCTL/TIPG вручную — `e1000_configure_tx` static) и передаёт 60-байтовый кадр:
       дескриптор → TDT → карта выносит его DMA'ом, ставит DD-бит. Проверено (x86): `setup_all_tx_resources
       → ring dma=0x13e3000` (реальный физ-адрес), `TX: desc0.status=0x01 DD=1 TDH=1 — OK`. Первое
       использование DMA-cap портированным Linux-драйвером. Регрессии чисты; ядро не менялось. Дальше — RX-кольцо
       (`e1000_setup_all_rx_resources`+`alloc_rx_buffers`, skb в DMA) + IRQ (`request_irq`↔задача на
       `vsys_irq_wait`, start_cap 2) + ISR/NAPI, мост RX/TX ↔ `net-srv` → `ping` через ПОРТИРОВАННЫЙ e1000;
       тот же конвейер закроет Atheros/EHCI/wifi X54C. ← следующее
   - Своими силами ещё: EHCI, NVMe, UEFI-GOP (framebuffer). GPU-3D = порт Linux DRM+Mesa (гора).
   - TCP + DHCP/конфиг (настоящая сеть на реальной LAN, не только QEMU-SLIRP).
   - Без IOMMU dma-cap = «DMA куда угодно» (драйвер доверенный); настоящая изоляция — потом.

## Связано
- [[todo]] · [[posix-personality]] · [[platform]] · [[void-pkg]] · [[commit-policy]]
