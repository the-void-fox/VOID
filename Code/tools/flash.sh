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
#   Code/tools/flash.sh /dev/sdX                  собрать образ из текущего ядра и записать
#   Code/tools/flash.sh /dev/sdX образ.img        записать готовый образ
#   Code/tools/flash.sh --grow /dev/sdX           + растянуть store на весь носитель
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

grow=0; assume_yes=0; force=0
args=()
for a in "$@"; do
    case "$a" in
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
собери: nix-shell --run 'cd Code && cargo build --release --target x86_64-unknown-none'"; exit 1; }
    # Образ собираем РЯДОМ СО СБОРКОЙ, а не в /tmp: на NixOS `/tmp` — это tmpfs, то есть
    # оперативная память, и семисотмегабайтный образ туда просто не влезает («на устройстве не
    # осталось свободного места» — при живом диске на десятки гигабайт).
    tmpdir="$repo/Code/target"
    mkdir -p "$tmpdir"
    free_mb=$(df -Pm "$tmpdir" | awk 'NR==2 {print $4}')
    if [ "${free_mb:-0}" -lt 800 ]; then
        echo "мало места под образ в $tmpdir: свободно ${free_mb} МиБ, нужно ~800"
        exit 1
    fi
    tmp="$tmpdir/void-flash-$$.img"
    img="$tmp"
    echo "собираю образ из $(basename "$kernel") …"
    # 700 МиБ: хватает store под пакеты и всё ещё быстро пишется. Растянуть на весь
    # носитель — ключ --grow (ядро берёт размер store из таблицы разделов).
    "$here/../boot/mkdisk.sh" "$kernel" "$img" 700 >/dev/null
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

if [ "$grow" = "1" ]; then
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
echo "готово. На $dev теперь чистая VOID: свой store, свой конфиг, ничего от прошлой версии."
echo "Первая загрузка сеет поколения заново; графический режим включается так:"
echo "  init-config → ved /etc/system/terminal.vv → mode = \"wm\" → rebuild → перезагрузка"
