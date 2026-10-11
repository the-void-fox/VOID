#!/usr/bin/env bash
# mkboot.sh — собрать ЖИВОЙ загрузочный ISO/USB-образ VOID (Вехи 41, 48, 171).
#
# VOID слинкован с multiboot2-заголовком, поэтому GRUB грузит ядро напрямую. Скрипт кладёт
# собранное ядро и grub.cfg в дерево, делает GRUB-rescue ISO (он же годится для записи на USB) и
# вкладывает в него ОБРАЗ ДИСКА модулем `module2`.
#
# ── Почему образ диска едет внутрь ISO ───────────────────────────────────────────────────────
#
# У этого модуля два применения сразу, и оба важные:
#
#   1) **живой носитель** (Веха 171): у компакт-диска нет записываемого раздела, а store — это и
#      есть система (поколения, конфиг, программы). Без записи VOID не умеет ни `rebuild`, ни
#      `switch`, ни поставить пакет, и загрузка кончается водопадом отказов. Поэтому, не найдя
#      настоящего диска, ядро берёт носителем ЭТОТ образ прямо в оперативной памяти. Работает
#      всё; изменения живут до перезагрузки, и система говорит об этом на загрузке вслух;
#   2) **источник установки** (Веха 48): `install` разворачивает его же на SATA-диск.
#
# Один файл на оба — специально: расходиться «тому, что показывают» и «тому, что ставят» негде.
#
# ── Почему образ ГИБРИДНЫЙ: BIOS и UEFI в одном файле (Веха 226) ─────────────────────────────
#
# На машинах новее CSM в прошивке обычно вырезан, и образ, умеющий только BIOS, там не грузится
# вовсе — не доходит даже до нашего ядра. Поэтому в ISO два входа загрузки сразу: BIOS через
# El Torito (как было) и UEFI через раздел ESP, который собирает `mkesp.sh`.
#
# Гибрид `grub-mkrescue` сам не делает: в nixpkgs GRUB собран под ОДНУ платформу, у нас
# `i386-pc`, а EFI-платформа живёт в отдельном пакете, и в одном каталоге они не встречаются.
# Зато всё, чего `grub-mkrescue` не узнал в своих аргументах, он отдаёт `xorriso` — этим и
# добавлены оба UEFI-входа. **Без `--` перед ними**: двойной дефис выключает у `xorriso` режим
# `mkisofs`, и дальше он не понимает ни `-e`, ни `--interval:` (проверено — падает вслух).
#
# ESP подаётся ДВАЖДЫ нарочно, и это не дубль:
#   * `-append_partition 2 0xef` — настоящий раздел в образе, по нему грузятся прошивки, когда
#     ISO записан на ФЛЕШКУ (а именно так его и носят);
#   * `-eltorito-alt-boot -e --interval:appended_partition_2:all::` — второй вход El Torito на тот
#     же самый раздел, по нему грузятся прошивки, когда образ подан приводом.
#
# ── Использование (из корня репозитория) ─────────────────────────────────────────────────────
#
#   Code/tools/run.sh --build-only                    # собрать ядро (release)
#   nix-shell shell.nix --run 'Code/boot/mkboot.sh'   # → Code/boot/void.iso
#
#   Запись на флешку (СОТРЁТ ЕЁ ЦЕЛИКОМ — проверь букву диска!):
#   sudo dd if=Code/boot/void.iso of=/dev/sdX bs=4M status=progress && sync
#
#   Проверка в QEMU — ТОЛЬКО с графикой и памятью: система поднимается в оконный режим, а модуль
#   целиком лежит в RAM (см. LIVE_MB ниже).
#   qemu-system-x86_64 -machine q35 -m 1280M -cdrom Code/boot/void.iso -boot d \
#       -device virtio-rng-pci,disable-legacy=on -accel kvm -cpu host
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"          # Code/boot
# Release, а не debug: ISO — это то, что отдают людям. Отладочная сборка втрое больше и заметно
# медленнее, а перепутать их легко — раньше здесь стояла именно она.
kernel="${1:-$here/../target/x86_64-unknown-none/release/void-kernel}"
[ -f "$kernel" ] || {
    echo "нет ядра: $kernel"
    echo "собери: Code/tools/run.sh --build-only   (или cargo build --release --target x86_64-unknown-none)"
    exit 1
}

# Сколько места отдать store на живом носителе, МиБ. Умолчание `mkdisk.sh` (8 МиБ) годится для
# установочного модуля, но не для работы: первый же посев занимает около четырёх, а дальше
# человек ставит пакеты, правит конфиг и коммитит поколения. 64 МиБ — с запасом в пятнадцать раз
# и всё ещё маленький ISO. Помни: модуль едет В ПАМЯТЬ целиком, поэтому «побольше» здесь не
# бесплатно.
LIVE_MB="${VOID_LIVE_MB:-64}"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/boot/grub"
cp "$kernel" "$staging/boot/void-kernel"
cp "$here/grub.cfg" "$staging/boot/grub/grub.cfg"

kernel_mb=$(( ( $(stat -c%s "$kernel") + 1048575 ) / 1048576 ))
"$here/mkdisk.sh" "$kernel" "$staging/boot/void-disk-boot.img" \
    $(( 1 + kernel_mb + 3 + LIVE_MB )) >/dev/null

# ESP — отдельным скриптом, потому что он нужен ещё и установке (см. mkesp.sh).
esp="$staging/../esp-$$.img"
"$here/mkesp.sh" "$esp" >/dev/null

# Ошибки НЕ глушим. Раньше здесь стоял `2>/dev/null`, и это ровно та молчаливая поломка, которую
# в этом проекте ловят месяцами: образ собирался «успешно» с потерянным входом загрузки.
grub-mkrescue -o "$here/void.iso" "$staging" \
    -append_partition 2 0xef "$esp" \
    -eltorito-alt-boot -e --interval:appended_partition_2:all:: -no-emul-boot
rm -f "$esp"

# Проверяем, что в образе ОБА входа, а не только тот, что собрался. Пустой или одинокий список —
# это собранный-но-негодный образ, и узнать об этом надо здесь, а не на чужой машине.
entries="$(xorriso -indev "$here/void.iso" -report_el_torito plain 2>/dev/null \
           | grep -c 'El Torito boot img' || true)"
[ "$entries" = "2" ] || {
    echo "в образе входов загрузки: $entries, а должно быть 2 (BIOS + UEFI)"
    exit 1
}
size_mb=$(( ( $(stat -c%s "$here/void.iso") + 1048575 ) / 1048576 ))
echo "готово: $here/void.iso (${size_mb} МиБ, store живёт в памяти на ${LIVE_MB} МиБ)"
echo "  на USB:  sudo dd if=$here/void.iso of=/dev/sdX bs=4M status=progress && sync"
echo "  входы загрузки: BIOS + UEFI (проверено по таблице El Torito)"
echo "  в QEMU (BIOS): qemu-system-x86_64 -machine q35 -m 1280M -cdrom $here/void.iso -boot d -accel kvm -cpu host"
echo "  в QEMU (UEFI): Code/tools/uefirun.py $here/void.iso <каталог-выхода>"
