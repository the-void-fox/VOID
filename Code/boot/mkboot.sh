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

grub-mkrescue -o "$here/void.iso" "$staging" 2>/dev/null
size_mb=$(( ( $(stat -c%s "$here/void.iso") + 1048575 ) / 1048576 ))
echo "готово: $here/void.iso (${size_mb} МиБ, store живёт в памяти на ${LIVE_MB} МиБ)"
echo "  на USB:  sudo dd if=$here/void.iso of=/dev/sdX bs=4M status=progress && sync"
echo "  в QEMU:  qemu-system-x86_64 -machine q35 -m 1280M -cdrom $here/void.iso -boot d -accel kvm -cpu host"
