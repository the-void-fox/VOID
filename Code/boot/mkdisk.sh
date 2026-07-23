#!/usr/bin/env bash
# mkdisk.sh — собрать загрузочный ОБРАЗ ДИСКА VOID (Веха 48): GRUB (i386-pc) + ядро в
# FAT-разделе p1, раздел p2 (тип 0x9f) под store VOID. БЕЗ root (sfdisk/mtools/grub-bios-setup
# работают прямо с файлом-образом). Два применения:
#   1) тест boot-from-disk в QEMU (эталон, что VOID грузится с диска);
#   2) исходник загрузочного образа, который in-VOID `install` пишет на SATA (этап 2).
#
# Использование (в nix-shell с grub2 mtools util-linux):
#   nix-shell -p grub2 mtools util-linux --run 'Code/boot/mkdisk.sh [ядро] [out.img]'
#   Тест:  qemu-system-x86_64 -machine q35 -m 512M -device ich9-ahci,id=a \
#            -drive if=none,id=d,file=Code/boot/void-disk-boot.img,format=raw \
#            -device ide-hd,drive=d,bus=a.0 -boot c -nographic
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
kernel="${1:-$here/../target/x86_64-unknown-none/release/void-kernel}"
out="${2:-$here/void-disk-boot.img}"
grubdir="$(dirname "$(command -v grub-mkimage)")/../lib/grub/i386-pc"
[ -f "$kernel" ] || { echo "нет ядра: $kernel (собери cargo build --release --target x86_64-unknown-none)"; exit 1; }
[ -f "$grubdir/boot.img" ] || { echo "нет GRUB i386-pc в $grubdir"; exit 1; }

# Размер образа мал по умолчанию: он же едет модулем multiboot2 на install-ISO (GRUB грузит его
# в RAM), а установщик всё равно растянет p2 на реальный диск. Больший размер (для standalone-теста
# boot-from-disk) — 3-м аргументом.
DISK_MB="${3:-12}"
P1_START=2048  # 1 МиБ — начало p1 (в MBR-зазоре 1..2047 живёт core.img GRUB)
P1_SIZE=8192   # 4 МиБ FAT под GRUB + ядро (в секторах) — хватает ядру ~1.5 МиБ
P2_START=$((P1_START + P1_SIZE)) # 10240 — загрузочный префикс = первые 5 МиБ (их пишет установщик)

# 1) пустой образ
rm -f "$out"
dd if=/dev/zero of="$out" bs=1M count="$DISK_MB" status=none

# 2) таблица разделов MBR: p1 FAT16 (bootable), p2 VOID store (тип 0x9f, остаток)
sfdisk --quiet "$out" <<EOF
label: dos
${P1_START},${P1_SIZE},c,*
${P2_START},,9f
EOF

# 3) p1 = FAT16, положить ядро + grub.cfg (mtools по смещению @@offset — без монтирования)
off=$((P1_START * 512))
mformat -i "$out@@${off}" -F ::
mmd -i "$out@@${off}" ::/boot ::/boot/grub
mcopy -i "$out@@${off}" "$kernel" ::/boot/void-kernel
cfg="$(mktemp)"
cat > "$cfg" <<'CFG'
set timeout=0
set default=0
menuentry "VOID" {
    multiboot2 /boot/void-kernel
    boot
}
CFG
mcopy -i "$out@@${off}" "$cfg" ::/boot/grub/grub.cfg
rm -f "$cfg"

# 4) GRUB core.img: модули для чтения FAT-раздела + multiboot2; префикс — на /boot/grub p1
tmpd="$(mktemp -d)"
grub-mkimage -O i386-pc -p "(hd0,msdos1)/boot/grub" -o "$tmpd/core.img" \
    biosdisk part_msdos fat multiboot2 normal configfile
coresects=$(( ($(stat -c%s "$tmpd/core.img") + 511) / 512 ))

# 5) Установка GRUB ВРУЧНУЮ (grub-bios-setup на NixOS спотыкается о tmpfs при скане mount'ов):
#    boot.img → сектор 0 (сохраняя таблицу разделов в 440..511), kernel_sector(0x5c)=LBA 1;
#    core.img → сектор 1.., блок-лист в секторе 1 (start=2, len=coresects-1).
write_le() { # файл смещение значение число_байт
    local f=$1 off=$2 val=$3 n=$4 i b
    for ((i = 0; i < n; i++)); do
        b=$(((val >> (8 * i)) & 0xff))
        printf "$(printf '\\x%02x' "$b")" | dd of="$f" bs=1 seek=$((off + i)) conv=notrunc status=none
    done
}
dd if="$grubdir/boot.img" of="$out" bs=1 count=440 conv=notrunc status=none
write_le "$out" 92 1 8 # 0x5c — LBA первого сектора core.img
dd if="$tmpd/core.img" of="$out" bs=512 seek=1 conv=notrunc status=none
write_le "$out" $((512 + 500)) 2 8 # блок-лист: старт остатка core.img (сектор 2)
write_le "$out" $((512 + 508)) $((coresects - 1)) 2 # длина остатка в секторах
rm -rf "$tmpd"
echo "готово: $out  (p1 FAT: GRUB+ядро · p2 тип 0x9f: store VOID)"
