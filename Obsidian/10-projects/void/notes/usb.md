---
title: USB — xHCI, клавиатура и накопитель (Вехи 50, 196)
created: 2026-07-01
tags: [project/void, topic/driver, topic/usb, topic/xhci, topic/hid, topic/storage, topic/hardware]
status: done
verified: 2026-09-13
---

# Веха 50 — USB (xHCI + HID-клавиатура)

Самый большой драйвер во всём проекте: полный USB-стек — контроллер, перечисление устройства,
класс-драйвер. Раньше клавиатура на металле была только PS/2 (или USB через BIOS-legacy в наш
`ps2.rs`). Теперь VOID сам говорит с контроллером xHCI и HID-клавиатурой. Идёт тремя частями,
каждая проверена в QEMU (`-device qemu-xhci -device usb-kbd`, ввод через QMP `send-key`). Опрос,
без прерываний — как все наши драйверы; все кольца/буферы — обнулённые фреймы [[frame-freeing|frame]]
(RAM отображена идентично, контроллер читает их DMA; на x86 DMA когерентен). **x86-only** (на
riscv/QEMU-virt xHCI нет — модуль-заглушка). Файл — `kernel/src/xhci.rs`, поиск — `arch::probe_xhci`
(PCI класс 0c/03/30).

## Часть A — контроллер (HCD), машинерия колец

Регистры cap/op/runtime/doorbell (смещения из `CAPLENGTH`/`DBOFF`/`RTSOFF`). Сброс контроллера,
`MaxSlotsEn`, **DCBAA** (массив контекстов устройств), **кольцо команд** (TRB по 16 Б, Link-TRB
заворотом с Toggle Cycle), **кольцо событий** + ERST, запуск (`R/S`). Порты: детект/сброс
подключённых. Ключевое доказательство — **Enable Slot**: команда в кольцо → звонок в doorbell 0 →
Command Completion Event в кольце событий (пропуская попутные Port Status Change) → валидный slot id.
Producer/Consumer Cycle State — душа колец xHCI. Проверено: «Enable Slot → 1».

## Часть B — перечисление устройства

**Address Device**: input-контекст (Input Control + Slot + EP0), контекст устройства в `DCBAA[slot]`,
TR-кольцо EP0; slot context (порт, скорость), EP0 context (MPS по скорости, EPType=Control, CErr=3,
TR dequeue). **Control-трансферы по EP0**: Setup(IDT) + Data(IN) + Status(OUT,IOC), звонок EP0
(DCI 1), Transfer Event; `GET_DESCRIPTOR` читает дескриптор. Проверено: «устройство 0627:0001
class=0» — клавиатура QEMU (класс HID объявлен на уровне интерфейса, поэтому deviceClass=0).

## Часть C — HID boot-клавиатура

Разбор **config-дескриптора** (интерфейс класса HID + его interrupt-IN эндпоинт: адрес, MPS,
интервал), `SET_CONFIGURATION`, `SET_PROTOCOL(boot)`, **Configure Endpoint** (interrupt-IN в контекст
устройства, `EPType=Interrupt IN`), Normal-TRB на interrupt-кольцо. Опрос (`poll_hid` из
`console_drain`): Transfer Event → 8-байтный boot-репорт `[mods, resv, k0..k5]` → НОВЫЕ нажатия
(сравнение с прошлым репортом) → `hid_to_ascii` (HID Usage → ASCII, Shift) → `arch::usb_key` в то же
кольцо консоли, что PS/2/COM1 — vsh не различает источник.

**Тонкость опроса при простое:** у USB нет прерывания. В режиме сна-до-ввода (`wait_stdin`) ядро
маскировало таймер и спало на HLT до IRQ консоли — USB-нажатия терялись бы. Фикс: `irq_mask_stdin`
держит таймер ВКЛ, когда поднята USB-клавиатура (`xhci::has_keyboard`) — тики будят HLT и опрашивают
её (~квант). Проверено: набор `p/w/d/Enter` (репорты 0x13/0x1a/0x07/0x28) → vsh печатает «pwd» и
выполняет (`/`).

## Часть D — накопитель (Веха 196)

Флешка стала блочным устройством: носителем store, целью установки и строкой в окне установщика.
Транспорт **BOT** (CBW → данные → CSW поверх двух bulk-эндпоинтов), поверх него SCSI: TEST UNIT
READY, READ CAPACITY(10), READ(10)/WRITE(10). Устройств теперь перечисляется НЕСКОЛЬКО (до
четырёх): раньше драйвер брал первый порт с устройством, и клавиатура с флешкой исключали друг
друга — какая раньше в списке портов, та и работала.

Проверено целиком: установка на флешку → загрузка С НЕЁ → файл переживает перезагрузку; и всё
это на машине без PS/2, с клавиатурой по USB. Подробности и найденные ошибки — в [[v02-drivers]].

**Что сломалось за время, пока USB никто не гонял** (стенда с USB не было с Вехи 50): заворот
колец (Link-TRB с чужим битом цикла), чтение физического адреса как виртуального в `poll_hid`,
отсутствие событий клавиатуры для композитора, один буфер приёма вместо восьми и общее кольцо
событий, из которого накопитель съедал репорты клавиатуры. Урок тот же, что в [[void-notes-trust]]:
«проверено» относится к тому дню, а не к сегодняшнему коду, — и если проверку нельзя повторить
сценарием, она протухает молча.

## Ограничения / осторожно

- **x54C = Sandy Bridge → EHCI, не xHCI.** Этот драйвер поднимет USB в QEMU и на машинах Ivy Bridge+,
  но НЕ на конкретном ноуте (там нужен EHCI — отдельный контроллер). Как и e1000 vs Atheros: это
  правильный ПЕРВЫЙ USB-контроллер (чистая спека, тестируемо), не финал для X54C.
- До четырёх устройств на корневых портах; **хабов нет** — через хаб устройства не видны вовсе.
  Мыши по USB нет (PS/2 есть). Опрос, не прерывания. Клавиатура — boot-протокол, US-раскладка.
  Накопитель — один LUN, блок 512 (при другом отказываемся вслух), по одному сектору за команду.

## Файлы

- `kernel/src/arch/x86_64/pci.rs` — `probe_xhci` (PCI 0c/03/30, BAR0).
- `kernel/src/xhci.rs` — HCD (A), перечисление (B), HID (C), накопитель (D): кольца, команды,
  control-трансферы, Configure Endpoint, `poll_hid`/`take_report`, `hid_to_ascii`/`hid_to_sym`,
  `has_keyboard`, BOT+SCSI (`scsi`, `read_capacity`, `rw_sector`) и интерфейс носителя/цели.
- `kernel/src/object.rs`, `kernel/src/install.rs`, `kernel/src/main.rs` — флешка как носитель
  store (последним по порядку: внутренний диск машины важнее) и как цель установки (диск 200).
- `Code/tools/qemu-machine.sh` — стенд: `VOID_QEMU_USB=kbd|disk:<путь>|boot:<путь>`,
  `VOID_QEMU_NO_PS2=1`; `Code/tools/screenrun.py` — глаголы `usb` и `usbhotkey`.
- `kernel/src/arch/x86_64/mod.rs` — `console_drain` → `xhci::poll`; `usb_key`; `irq_mask_stdin`
  держит таймер при USB-клавиатуре.
- `kernel/src/main.rs` — `xhci::init()` (модуль cfg-gated на x86; riscv — заглушка).

## Связано
- [[ahci]] · [[e1000]] (тот же приём: реальный драйвер опросом, DMA-фреймы) · [[platform]] (PS/2, VGA) · [[known-gaps]] · [[todo]]
