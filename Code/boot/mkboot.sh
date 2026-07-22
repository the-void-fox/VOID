#!/usr/bin/env bash
# mkboot.sh — собрать загрузочный ISO/USB-образ VOID для реального x86-железа (Веха 41).
#
# VOID слинкован с multiboot1-заголовком, поэтому GRUB грузит ядро напрямую. Скрипт кладёт
# собранное ядро и grub.cfg в дерево и делает GRUB-rescue ISO (он же годится для записи на USB).
#
# Использование (из корня репозитория, в nix-shell):
#   cargo build --target x86_64-unknown-none          # собрать ядро (или --release)
#   nix-shell -p grub2 xorriso --run 'Code/boot/mkboot.sh'
#   # → Code/boot/void.iso
#
#   Запись на флешку (СОТРЁТ ЕЁ ЦЕЛИКОМ — проверь букву диска!):
#   sudo dd if=Code/boot/void.iso of=/dev/sdX bs=4M status=progress && sync
#
# Проверка в QEMU (тот же путь, что на железе — GRUB+multiboot):
#   qemu-system-x86_64 -machine q35 -m 512M -cdrom Code/boot/void.iso -nographic
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"          # Code/boot
kernel="${1:-$here/../target/x86_64-unknown-none/debug/void-kernel}"
[ -f "$kernel" ] || { echo "нет ядра: $kernel (собери cargo build --target x86_64-unknown-none)"; exit 1; }

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/boot/grub"
cp "$kernel" "$staging/boot/void-kernel"
cp "$here/grub.cfg" "$staging/boot/grub/grub.cfg"

grub-mkrescue -o "$here/void.iso" "$staging" 2>/dev/null
echo "готово: $here/void.iso  (записать на USB: sudo dd if=$here/void.iso of=/dev/sdX bs=4M; sync)"
