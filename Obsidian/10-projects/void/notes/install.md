---
title: Веха 48 — установка на диск. live-USB → install → загрузка с диска
created: 2026-07-01
tags: [project/void, topic/installer, topic/boot, topic/ahci, topic/grub, topic/hardware]
status: done
---

# Веха 48 — установщик: VOID ставит себя на диск

После [[ahci]] (Веха 47) store умеет жить на реальном SATA-диске, но система всё ещё грузилась
**с USB** (GRUB-ISO), а на диске был только store. Эта веха закрывает круг: система, загруженная
с USB, **сама раскладывает себя на диск** и дальше грузится с него — как live-USB инсталлятор Linux.

## Развилка, которую пришлось решить

Store занимал диск **с сектора 0** (там суперблок VOID). А чтобы грузиться с диска, на нём же
должен жить загрузчик — но GRUB не умеет читать наш store. Значит диск надо **разбить на разделы**.

## Этап 1 — фундамент: store на разделе + загрузка с диска

- **AHCI читает MBR** ([[ahci]], `ahci.rs`): при инициализации — сектор 0; если есть раздел типа
  `0x9f` (наш store, `VOID_STORE_TYPE`), store живёт на его смещении (`base` LBA прибавляется к
  каждому сектору), а `capacity` = размер раздела. Нет MBR/раздела — store с сектора 0 на весь диск
  (обратная совместимость с `void-disk.img` в QEMU). Абсолютные `write_abs`/`read_abs` — для
  установщика (пишет НЕ в раздел, а в начало ДИСКА).
- **Загрузочный образ диска** — `Code/boot/mkdisk.sh` (БЕЗ root: `sfdisk`+`mtools`+ручная установка
  GRUB): p1 FAT16 (GRUB i386-pc + ядро + grub.cfg), p2 тип `0x9f` (store). GRUB `grub-bios-setup` на
  NixOS спотыкается о tmpfs при скане mount'ов → ставим GRUB **вручную**: `boot.img`→сектор 0
  (сохраняя таблицу разделов в 440..511), `kernel_sector`(0x5c)=LBA 1; `core.img`→зазор 1.., блок-лист
  в секторе 1 (start=2, len=coresects−1). Проверено: QEMU грузит образ с диска (GRUB→ядро→store на p2),
  запись переживает ребут, загрузочная область не тронута (store на p2 её не задевает).

## Этап 2 — установщик in-VOID

- **Образ едет модулем multiboot2.** grub.cfg install-ISO: `multiboot2 /boot/void-kernel` +
  `module2 /boot/void-disk-boot.img`. GRUB кладёт образ в RAM; ядро находит его в тегах multiboot2
  (`arch::boot_module`, тег type 3). Так **установленное ядро == работающему** (один бинарь в p1
  образа), без встраивания образа в ядро (это было бы циклично: ядро содержит образ, содержащий
  ядро…). Модуль уберегаем от bump-аллокатора фреймов: `frame::reserve_boot_module` поднимает старт
  `NEXT` за конец модуля (GRUB кладёт его в свободную RAM за образом ядра). `mkboot.sh` кладёт образ
  на ISO модулем, `mkdisk.sh` по умолчанию мал (p1 4 МиБ → префикс 5 МиБ, ~12 МиБ образ).
- **vsh `install`** → `SYS_INSTALL(store_cap)` (№30, гейт: store-cap с WRITE — тот же, что у shell'а
  `store:xw`). Ядро (`install.rs`): (1) пишет загрузочный префикс образа (сектора 0..начало p2 =
  MBR+зазор+p1) на диск через `write_abs`; (2) правит в MBR p2 (store) — растягивает на весь реальный
  диск (`num_sectors = total − p2_start`); (3) обнуляет начало p2 → на ребуте store решит «пусто» и
  засеется; (4) **замораживает** текущий store (`object::freeze` → `Disk::write` no-op), чтобы
  group-commit не затёр свежий образ до перезагрузки. Дальше — вынуть USB, ребут.

## Проверка (QEMU q35, ich9-ahci, пустой 64-МиБ диск)

Загрузка install-ISO с чистого SATA → `install`: «VOID установлен на диск (store с сектора 10240);
заморожен». Таблица разделов после: p1 FAT (2048, 8192), **p2 `0x9f` (10240, 120832 — растянут на весь
диск)**. Ребут **только с SATA** (без ISO): GRUB→ядро→store на p2 (первый запуск, засеялся)→vsh;
`echo>proof` → ещё ребут → `cat proof` вернул текст (поколение 2 загружено с диска). Регрессия
обычного `cargo run` (PVH, virtio) чиста — модуля нет, `install` сказал бы «нет образа».

## Осторожно / ограничения

- **`install` СТИРАЕТ ВЕСЬ ДИСК** (пишет с сектора 0, p2 = остаток) — сносит таблицу разделов и
  существующую ОС. Только пустой/запасной диск. BIOS в режиме AHCI.
- Разметка фиксированная: 1 диск, MBR (не GPT), p1 4 МиБ FAT, p2 = остаток под store. Мульти-диск,
  выбор диска, UEFI/GPT — не реализованы.
- Установка = полное стирание; «обновление на месте» (сохранить старый store) пока нет.

## Файлы

- `kernel/src/ahci.rs` — MBR-парсинг (`base`/`capacity`/`total`), `write_abs`/`read_abs`.
- `kernel/src/install.rs` — логика установки (префикс → диск, правка MBR, очистка store, freeze).
- `kernel/src/object.rs` — `freeze`/`FROZEN` (заморозка записи после установки).
- `kernel/src/arch/x86_64/mod.rs` — `boot_module` (тег multiboot2 type 3); `frame.rs` — `reserve_boot_module`.
- `kernel/src/proc.rs` — `SYS_INSTALL` (№30, гейт store WRITE).
- `programs/user/src/{lib.rs,bin/vsh.rs}` — `sys::install` + команда `install`.
- `Code/boot/mkdisk.sh` (образ диска), `mkboot.sh` (+модуль), `grub.cfg` (+`module2`).

## Связано
- [[ahci]] (SATA-диск) · [[platform]] (реальный x86, GRUB) · [[declarative-init]] (что грузится) · [[known-gaps]] · [[todo]]
