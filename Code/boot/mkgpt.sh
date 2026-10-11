#!/usr/bin/env bash
# mkgpt.sh — собрать образ диска с разметкой **GPT** и загрузкой по UEFI (Веха 226.6).
#
# ── Чем отличается от mkdisk.sh ──────────────────────────────────────────────────────────────
#
# `mkdisk.sh` делает диск по-старому: MBR, GRUB в зазоре перед первым разделом, загрузка через
# BIOS. Здесь всё иначе и проще: таблица GPT, первый раздел — ESP (его читает сама прошивка),
# второй — store VOID. Зазора нет, `grub-bios-setup` не нужен, в MBR лежит только защитная
# запись.
#
# ── Зачем этот скрипт существует ─────────────────────────────────────────────────────────────
#
# Он не для людей, а для ПРОВЕРКИ: это эталон разметки, которую должен делать `install` на
# UEFI-машине (пункт A6 плана). Пока установщик её не умеет, другого способа прогнать чтение
# GPT ядром нет вовсе — а непроверенный разбор чужих байт в ядре это ровно тот класс, которым
# в этом проекте уже обожглись.
#
# Попутно он отвечает на вопрос, который иначе остался бы на веру: **читает ли ядро GPT на
# настоящем диске, а не только в юнит-тесте**.
#
# ── Где лежит ядро на установленном диске ────────────────────────────────────────────────────
#
# В ESP, рядом с `BOOTX64.EFI`. Больше ему лежать негде: второй раздел — store VOID, а его
# формата GRUB не знает и знать не будет. Поэтому ESP здесь не «маленький служебный раздел», а
# загрузочный том целиком: `/EFI/BOOT/BOOTX64.EFI`, `/boot/void-kernel`, `/boot/grub/grub.cfg`.
#
# ── Использование ────────────────────────────────────────────────────────────────────────────
#
#   nix-shell --run 'Code/boot/mkgpt.sh [ядро] [out.img] [МиБ]'
#   Code/tools/uefirun.py out.img ВЫХОД -- -device ich9-ahci,id=a \
#       -drive if=none,id=d,file=out.img,format=raw -device ide-hd,drive=d,bus=a.0
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
kernel="${1:-$here/../target/x86_64-unknown-none/release/void-kernel}"
out="${2:-$here/void-gpt.img}"
disk_mb="${3:-700}"

[ -f "$kernel" ] || { echo "нет ядра: $kernel"; exit 1; }

# Раскладка в секторах. ESP начинается с 2048 (1 МиБ) — там же, где его кладут все остальные:
# выравнивание на мегабайт важно SSD, а не нам, но ссориться с общим обычаем незачем.
P1_START=2048
esp_mb=$(( ( $(stat -c%s "$kernel") + 1048575 ) / 1048576 + 12 ))
P1_SIZE=$(( esp_mb * 2048 ))
P2_START=$(( P1_START + P1_SIZE ))
# Хвост: резервная копия GPT живёт в последних 33 секторах. Отдать разделу весь диск значит
# затереть её первым же коммитом store — и обнаружить это на чужой машине.
P2_SIZE=$(( disk_mb * 2048 - P2_START - 34 ))

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Содержимое ESP, кроме самого загрузчика: ядро и конфиг GRUB.
mkdir -p "$tmp/tree/boot/grub"
cp "$kernel" "$tmp/tree/boot/void-kernel"
# Конфиг БЕЗ строки `module2`: загрузочный образ в память нужен живому носителю, а у диска
# store лежит на втором разделе — ядро найдёт его само по типу раздела.
sed '/module2/d' "$here/grub.cfg" > "$tmp/tree/boot/grub/grub.cfg"

"$here/mkesp.sh" "$tmp/esp.img" "$tmp/tree" >/dev/null
esp_bytes=$(stat -c%s "$tmp/esp.img")
[ "$esp_bytes" -le $(( P1_SIZE * 512 )) ] || {
    echo "ESP не влезает в раздел: $esp_bytes Б против $(( P1_SIZE * 512 ))"
    exit 1
}

rm -f "$out"
truncate --size="${disk_mb}M" "$out"
sfdisk --quiet --label gpt "$out" >/dev/null <<EOF
start=$P1_START, size=$P1_SIZE, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="ESP"
start=$P2_START, size=$P2_SIZE, type=564F4944-0000-4000-A000-564F49445354, name="VOID store"
EOF

dd if="$tmp/esp.img" of="$out" bs=512 seek="$P1_START" conv=notrunc status=none

# Store не трогаем вовсе: нули значат «диск пуст», и ядро честно скажет «первый запуск» и
# посеет систему само. Это и проверка записи заодно.
echo "готово: $out (${disk_mb} МиБ · GPT · ESP ${esp_mb} МиБ с ядром · store $(( P2_SIZE / 2048 )) МиБ)"
echo "  проверить: Code/tools/uefirun.py $out ВЫХОД -- -device ich9-ahci,id=a \\"
echo "      -drive if=none,id=d,file=$out,format=raw -device ide-hd,drive=d,bus=a.0"
