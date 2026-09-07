#!/usr/bin/env bash
# flash.sh — записать VOID на съёмный носитель ПОЛНОСТЬЮ ЧИСТО (Веха 121.1).
#
# Зачем отдельный инструмент. Обычное `dd` образа поверх флешки оставляет ровно ту ловушку,
# на которой владелец потерял день: старый store переживает обновление ядра, а вместе с ним —
# старый конфиг поколения. Система при этом выглядит рабочей, просто ведёт себя как прошлая
# версия: клавиши старые, режим старый, новые действия «не работают». Ошибка ТИХАЯ.
#
# Поэтому здесь всё стирается явно: подписи файловых систем, таблица разделов (включая
# резервную GPT в конце устройства), хвост старого store. После записи на носителе нет ничего,
# кроме нового образа.
#
# ── Что именно пишется (Веха 173) ───────────────────────────────────────────────────────────
#
# **ЖИВОЙ ISO** (`boot/mkboot.sh`), а не образ диска. Раньше писался образ диска, и получалась
# флешка, которая грузится, но неправильно:
#
#   1. **store с неё не читается.** У VOID нет драйвера USB-накопителей (xHCI умеет только
#      клавиатуру, EHCI не умеет ничего), поэтому загрузившись с флешки система не видит её
#      разделов вовсе — «диск не найден». Всё, что обещала строка «свой store, свой конфиг»,
#      доставалось внутреннему SATA-диску машины, а не носителю;
#   2. **на ней не было `install`.** Установщик сеется только на носителе, а признак носителя —
#      загрузочный модуль GRUB, которого у образа диска нет. То есть флешкой нельзя было
#      поставить систему — ровно то, ради чего её и пишут.
#
# У живого ISO обеих бед нет: store лежит в модуле, который GRUB привозит В ПАМЯТЬ, читать
# носитель после загрузки не нужно, и `install` на месте. Изменения живут до перезагрузки —
# это и есть живой носитель.
#
#   Code/tools/flash.sh /dev/sdX                  собрать живой ISO и записать
#   Code/tools/flash.sh /dev/sdX образ            записать готовый файл (ISO или образ диска)
#   Code/tools/flash.sh --disk /dev/sdX           собрать ОБРАЗ ДИСКА (см. выше, зачем — редко)
#   Code/tools/flash.sh --grow /dev/sdX           + растянуть store (только с --disk)
#   Code/tools/flash.sh --yes /dev/sdX            без вопроса (для своих скриптов)
#   Code/tools/flash.sh --force /dev/sdX          разрешить НЕсъёмный диск (осторожно!)
#
# Запускать под root (запись в блочное устройство): sudo Code/tools/flash.sh /dev/sdX
set -euo pipefail

# Недостроенный образ не должен оставаться лежать: он большой, а место, как выяснилось, кончается.
tmp=""
trap '[ -n "$tmp" ] && rm -f "$tmp"' EXIT

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

grow=0; assume_yes=0; force=0; raw_disk=0
args=()
for a in "$@"; do
    case "$a" in
        --disk)  raw_disk=1 ;;
        --grow)  grow=1 ;;
        --yes|-y) assume_yes=1 ;;
        --force) force=1 ;;
        -*) echo "неизвестный ключ: $a"; exit 2 ;;
        *) args+=("$a") ;;
    esac
done
[ "${#args[@]}" -ge 1 ] || { sed -n '3,20p' "$0"; exit 2; }

dev="${args[0]}"
img="${args[1]:-}"

# ── проверки, каждая из которых однажды кого-нибудь спасёт ───────────────────
[ -b "$dev" ] || { echo "не блочное устройство: $dev"; exit 1; }
case "$dev" in
    *[0-9]) if [ -e "/sys/class/block/$(basename "$dev")/partition" ]; then
                echo "$dev — это РАЗДЕЛ, а нужен диск целиком (например /dev/sdb, не /dev/sdb1)"; exit 1
            fi ;;
esac
[ "$(id -u)" = "0" ] || { echo "нужен root: sudo $0 $*"; exit 1; }

name="$(basename "$dev")"
removable="$(cat "/sys/block/$name/removable" 2>/dev/null || echo 0)"
if [ "$removable" != "1" ] && [ "$force" != "1" ]; then
    echo "$dev НЕ съёмный. Если это правда тот диск, повтори с --force."
    exit 1
fi
# Смонтированное — верный признак, что диск не тот (или что данные ещё нужны).
if grep -q "^$dev" /proc/mounts; then
    echo "на $dev есть смонтированные разделы — отмонтируй их сначала:"
    grep "^$dev" /proc/mounts | sed 's/^/  /'
    exit 1
fi

size_b=$(blockdev --getsize64 "$dev")
model="$(cat "/sys/block/$name/device/model" 2>/dev/null || echo '?')"
echo "устройство: $dev  ($model, $((size_b / 1024 / 1024)) МиБ, съёмный=$removable)"

# ── образ ────────────────────────────────────────────────────────────────────
tmp=""
if [ -z "$img" ]; then
    kernel="$repo/Code/target/x86_64-unknown-none/release/void-kernel"
    [ -f "$kernel" ] || { echo "нет ядра: $kernel
собери: Code/tools/run.sh --build-only"; exit 1; }
    if [ "$raw_disk" = 1 ]; then
        # Образ собираем РЯДОМ СО СБОРКОЙ, а не в /tmp: на NixOS `/tmp` — это tmpfs, то есть
        # оперативная память, и семисотмегабайтный образ туда просто не влезает («на устройстве
        # не осталось свободного места» — при живом диске на десятки гигабайт).
        tmpdir="$repo/Code/target"
        mkdir -p "$tmpdir"
        free_mb=$(df -Pm "$tmpdir" | awk 'NR==2 {print $4}')
        if [ "${free_mb:-0}" -lt 800 ]; then
            echo "мало места под образ в $tmpdir: свободно ${free_mb} МиБ, нужно ~800"
            exit 1
        fi
        tmp="$tmpdir/void-flash-$$.img"
        img="$tmp"
        echo "собираю ОБРАЗ ДИСКА из $(basename "$kernel") …"
        "$here/../boot/mkdisk.sh" "$kernel" "$img" 700 >/dev/null
    else
        echo "собираю живой ISO из $(basename "$kernel") …"
        "$here/../boot/mkboot.sh" "$kernel" >/dev/null
        img="$repo/Code/boot/void.iso"
    fi
fi
[ -f "$img" ] || { echo "нет образа: $img"; exit 1; }
img_b=$(stat -c%s "$img")
[ "$img_b" -le "$size_b" ] || { echo "образ ($img_b Б) больше носителя ($size_b Б)"; exit 1; }

if [ "$assume_yes" != "1" ]; then
    echo
    echo "ВСЁ СОДЕРЖИМОЕ $dev БУДЕТ УНИЧТОЖЕНО (включая store прошлой установки)."
    printf 'напиши путь устройства для подтверждения: '
    read -r confirm
    [ "$confirm" = "$dev" ] || { echo "не совпало — ничего не делаю"; exit 1; }
fi

# ── стирание ─────────────────────────────────────────────────────────────────
echo "стираю подписи и таблицы разделов …"
wipefs -a "$dev" >/dev/null
# Начало: MBR, загрузочный зазор, начало p1. Конец: резервная таблица GPT — она переживает
# перезапись начала и потом всплывает как «диск уже размечен».
dd if=/dev/zero of="$dev" bs=1M count=16 conv=fsync status=none
tail_mb=4
seek_mb=$(( size_b / 1024 / 1024 - tail_mb ))
[ "$seek_mb" -gt 16 ] && dd if=/dev/zero of="$dev" bs=1M count="$tail_mb" seek="$seek_mb" conv=fsync status=none

echo "пишу образ ($((img_b / 1024 / 1024)) МиБ) …"
dd if="$img" of="$dev" bs=4M conv=fsync status=progress

if [ "$grow" = "1" ] && [ "$raw_disk" != "1" ]; then
    echo "--grow пропущен: у живого ISO store лежит в памяти, растягивать на носителе нечего."
elif [ "$grow" = "1" ]; then
    # Ядро берёт начало и длину store из записи MBR (`ahci.rs`, тип 0x9f) — значит растянуть
    # store это просто исправить длину второго раздела. Область за образом уже обнулена, так
    # что store увидит пустое место, а не мусор.
    echo "растягиваю store на весь носитель …"
    start=$(sfdisk -d "$dev" | awk '/type=9f/ {gsub(",","",$4); print $4}')
    [ -n "$start" ] || { echo "не нашёл раздел store (тип 9f) — оставляю как есть"; start=""; }
    if [ -n "$start" ]; then
        sectors=$(( size_b / 512 ))
        printf 'start=%s, size=%s, type=9f\n' "$start" "$(( sectors - start ))" \
            | sfdisk --no-reread -N 2 "$dev" >/dev/null
    fi
fi

sync
echo
echo "готово. На $dev теперь чистая VOID, ничего от прошлой версии."
if [ "$raw_disk" = 1 ]; then
    echo "Это ОБРАЗ ДИСКА: с флешки его store не читается (нет драйвера USB-накопителей),"
    echo "и программы \`install\` на нём нет. Для флешки правильный носитель — живой ISO."
else
    echo "Загрузится сразу в оконный режим; store живёт в памяти, изменения — до перезагрузки."
    echo "Поставить на диск машины: команда \`install\` (Super+Return → install)."
    echo "После установки первая загрузка сама сеет /etc/system/*.vv — правь и \`rebuild\`."
fi
