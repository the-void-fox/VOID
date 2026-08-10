---
title: Запуск VOID в QEMU с записью (и почему ISO не годится)
created: 2026-08-02
tags: [project/void, topic/tooling, reference]
status: active
---

# Как гонять VOID в QEMU по-настоящему

`-cdrom void.iso` даёт устройство **только для чтения**. Store честно отказывается коммитить и
пишет «на диске прежнее консистентное поколение» — это не поломка, а правильный отказ: система
работает, но ничего не сохраняет (ни `rebuild`, ни правки конфига, ни пакеты). ISO — образ
УСТАНОВКИ, его дело загрузиться и записать себя на диск.

Для работы нужен образ ДИСКА:

```
nix-shell -p grub2 mtools util-linux --run \
  "Code/boot/mkdisk.sh Code/target/x86_64-unknown-none/release/void-kernel /var/tmp/void.img 700"

qemu-system-x86_64 -machine q35 -m 1280M -accel kvm \
  -device ich9-ahci,id=a \
  -drive if=none,id=d,file=/var/tmp/void.img,format=raw \
  -device ide-hd,drive=d,bus=a.0 -boot c \
  -device virtio-rng-pci,disable-legacy=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy=on
```

**`-accel kvm` обязателен, если смотрят на скорость.** Без него QEMU эмулирует каждую инструкцию
(TCG), и кадр композитора стоит там сотни миллисекунд — именно поэтому замеры анимации в
`screenrun.py` бесполезны как оценка плавности: там мерится эмулятор, а не VOID.

**Не класть образ в `/tmp`**: на NixOS это tmpfs, то есть оперативная память (см. `flash.sh`,
Веха 126.2). `/var/tmp` или каталог сборки.
