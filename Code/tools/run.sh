#!/usr/bin/env bash
# run.sh — запустить VOID в QEMU ОДНОЙ командой: собрать ядро, обновить образ, загрузиться.
#
#   Code/tools/run.sh                  собрать и запустить (x86_64, окно + serial здесь)
#   Code/tools/run.sh --fresh          пересобрать образ с нуля (store СТИРАЕТСЯ)
#   Code/tools/run.sh --riscv          вторая архитектура (текстовая консоль, без графики)
#   Code/tools/run.sh --headless       без окна QEMU — только консоль ядра в терминале
#   Code/tools/run.sh --net none       без сетевой карты вовсе
#   Code/tools/run.sh --script с.txt в/  СЦЕНАРНЫЙ прогон: клавиши, мышь, снимки (screenrun.py)
#   Code/tools/run.sh --build-only     только собрать ядро и обновить образ, не запускать
#   Code/tools/run.sh -- -device …     всё после `--` уходит в QEMU как есть
#
# Ключи: --fresh · --debug · --riscv · --no-build · --headless · --snapshot · --tcg
#        --net user|none|tap:<имя> · --mem 1280M · --img путь · --size 700
#        --script <сценарий> <каталог> · --build-only
#
# ── Зачем скрипт, если есть `cargo run` ──────────────────────────────────────────────────────
#
# `cargo run` грузит ядро ключом `-kernel` (PVH) — и **фреймбуфера при этом нет**: ни
# композитора, ни терминала на глифах, ни мыши. Графика VOID живёт только при загрузке через
# GRUB с ОБРАЗА ДИСКА (multiboot2 отдаёт тег фреймбуфера), а образ надо собрать, положить в него
# ядро и не потерять store. Это четыре команды с тремя ловушками, и все три уже кого-то стоили:
#
#   1. образ в `/tmp` — на NixOS это tmpfs, то есть ОПЕРАТИВНАЯ ПАМЯТЬ (700 МиБ туда не лезут);
#   2. пересборка образа ради нового ядра СТИРАЕТ store — вместе с конфигом поколения, которое
#      набирали руками, и с картинками, положенными мостом. Здесь по умолчанию заменяется
#      ТОЛЬКО ядро (mcopy в p1), а store остаётся;
#   3. без `-accel kvm` QEMU эмулирует каждую инструкцию, и кадр композитора стоит сотни
#      миллисекунд: «медленно» в таком прогоне не значит ничего.
#
# ── Почему сценарный прогон живёт ЗДЕСЬ, а не отдельной командой ────────────────────────────
#
# `--script` — тот же путь, только вместо окна QEMU поднимается `screenrun.py` (клавиши, мышь,
# снимки кадра). Вынести его отдельным скриптом значило бы завести ВТОРОЕ место, где ядро
# кладут в образ, — и однажды прогнать сценарий на прошлом ядре, ничего при этом не заметив.
# Так уже чуть не вышло: проверку правки делали образом, собранным до неё.
#
# Два VOID'а на одном проводе — `tools/netlab.py`; положить файл в store с хоста —
# `tools/void-store-import`; померить снимки после прогона — `tools/shot.py`.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
code="$(cd "$here/.." && pwd)"
repo="$(cd "$code/.." && pwd)"

arch=x86
profile=release
fresh=0
build=1
headless=0
snapshot=0
tcg=0
mem="${VOID_QEMU_MEM:-1280M}"
net="${VOID_QEMU_NET:-user}"
img="${VOID_IMG:-$code/target/void.img}"
size_mb=700
script=""
outdir=""
build_only=0
extra=()

# Аргументы запоминаем ДО разбора: ниже скрипт может перезапустить себя внутри nix-shell, а к
# тому месту `$@` уже съеден `shift`'ами. Ошибка была тихой ровно так, как это бывает хуже
# всего: перезапуск проходил, но БЕЗ ключей — `--headless` терялся, и вместо тихого прогона
# открывалось окно QEMU.
orig=("$@")

while [ $# -gt 0 ]; do
    case "$1" in
        --fresh)     fresh=1 ;;
        --debug)     profile=debug ;;
        --riscv)     arch=riscv ;;
        --no-build)  build=0 ;;
        --headless)  headless=1 ;;
        --snapshot)  snapshot=1 ;;
        --tcg)       tcg=1 ;;
        --net)       net="$2"; shift ;;
        --mem)       mem="$2"; shift ;;
        --img)       img="$2"; shift ;;
        --size)      size_mb="$2"; shift ;;
        --script)    script="${2:-}"; outdir="${3:-}"; shift 2 ;;
        --build-only) build_only=1 ;;
        --)          shift; extra=("$@"); break ;;
        # Шапка файла и есть справка — печатаем её до первой НЕ-комментарной строки, чтобы она
        # не расходилась с текстом при правках (номера строк расходятся всегда).
        -h|--help)   sed -n '2,/^[^#]/p' "$0" | sed '$d'; exit 0 ;;
        *)           echo "неизвестный ключ: $1 (--help)"; exit 2 ;;
    esac
    shift
done

say() { printf '  %s\n' "$*"; }

# Проверяем ДО сборки: ошибка в аргументах сценария, найденная после трёх минут `cargo build`,
# стоит этих трёх минут дважды.
if [ -n "$script" ]; then
    [ -n "$outdir" ] || { echo "--script хочет ДВА аргумента: <сценарий> <каталог-выхода>"; exit 2; }
    [ -f "$script" ] || { echo "нет сценария: $script (строки см. в шапке tools/screenrun.py)"; exit 2; }
    [ "$arch" = x86 ] || { echo "--script только для x86_64: на riscv в QEMU нет дисплея"; exit 2; }
fi

# ── окружение ────────────────────────────────────────────────────────────────────────────────
#
# Инструменты (qemu, mtools, grub, util-linux) живут в dev-shell'е репозитория, а Rust — в
# rustup на хосте (см. shell.nix, там же почему). Если скрипт позвали из обычной оболочки, он
# сам входит в nix-shell и перезапускает себя — иначе «одна команда» превращается в две.
if [ -z "${VOID_RUN_NIX:-}" ]; then
    missing=""
    for t in qemu-system-x86_64 qemu-system-riscv64 mcopy mdir sfdisk grub-mkimage; do
        command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
    done
    if [ -n "$missing" ]; then
        command -v nix-shell >/dev/null 2>&1 || {
            echo "нет на PATH:$missing — и nix-shell тоже нет."
            echo "поставь их или зайди в окружение: nix-shell $repo/shell.nix"
            exit 1
        }
        say "→ вхожу в nix-shell (не хватает:$missing)"
        exec nix-shell "$repo/shell.nix" --run "VOID_RUN_NIX=1 $(printf '%q ' "$0" ${orig[@]+"${orig[@]}"})"
    fi
fi
command -v cargo >/dev/null 2>&1 || { echo "нет cargo на PATH (rustup ставится на хосте)"; exit 1; }

flags=()
[ "$profile" = release ] && flags=(--release)

# ── riscv: там нет ни экрана, ни GRUB — только `cargo run` и консоль ─────────────────────────
#
# Отдельной веткой, а не «тем же кодом с другим таргетом»: на riscv (QEMU virt) дисплея нет
# вовсе, значит нет ни образа с GRUB, ни фреймбуфера, ни композитора. Диск при этом нужен —
# store живёт в нём, — и раннер из `.cargo/config.toml` ждёт его рядом с исходниками.
if [ "$arch" = riscv ]; then
    disk="$code/void-disk.img"
    [ "$fresh" = 1 ] && rm -f "$disk"
    if [ ! -f "$disk" ]; then
        # 512 МиБ, разрежённый файл (место занимает только записанное). Шестнадцати мегабайт из
        # старого комментария в `.cargo/config.toml` давно не хватает: один посев программ — это
        # больше тридцати корней, и store честно упирался в «НОСИТЕЛЬ ПОЛОН», отменяя коммиты.
        truncate -s 512M "$disk"
        say "создан пустой диск $disk (512 МиБ, разрежённый)"
    fi
    say "riscv64: текстовая консоль, графики на этой архитектуре нет. Выход: Ctrl-A, затем X"
    cd "$code"
    exec cargo run "${flags[@]}" --target riscv64gc-unknown-none-elf
fi

# ── сборка ───────────────────────────────────────────────────────────────────────────────────
kernel="$code/target/x86_64-unknown-none/$profile/void-kernel"
if [ "$build" = 1 ]; then
    say "сборка ядра (x86_64, $profile) …"
    ( cd "$code" && cargo build "${flags[@]}" --target x86_64-unknown-none )
fi
[ -f "$kernel" ] || {
    echo "нет ядра: $kernel"
    echo "собери его (или убери --no-build): cd $code && cargo build ${flags[*]} --target x86_64-unknown-none"
    exit 1
}

# ── образ ────────────────────────────────────────────────────────────────────────────────────

# Начало p1 в секторах — из таблицы разделов, а не из константы: образ мог собрать кто-то ещё
# (`flash.sh`, установщик), и молча промахнуться мимо раздела значит испортить его.
p1_start() {
    sfdisk -d "$1" 2>/dev/null | sed -n 's/^[^ ]*1 *: *start= *\([0-9][0-9]*\).*/\1/p' | head -1
}

# Собрать образ с нуля. Store при этом ПУСТОЙ — система посеет поколения заново.
make_image() {
    mkdir -p "$(dirname "$img")"
    free_mb=$(df -Pm "$(dirname "$img")" | awk 'NR==2 {print $4}')
    [ "${free_mb:-0}" -ge $((size_mb + 50)) ] || {
        echo "мало места под образ в $(dirname "$img"): свободно ${free_mb} МиБ, нужно ~$((size_mb + 50))"
        exit 1
    }
    rm -f "$img"
    "$code/boot/mkdisk.sh" "$kernel" "$img" "$size_mb" >/dev/null
    say "образ собран: $img (${size_mb} МиБ), store ПУСТОЙ"
}

[ "$fresh" = 1 ] && rm -f "$img"
if [ ! -f "$img" ]; then
    make_image
else
    # Меняем ТОЛЬКО ядро: store с конфигом поколения, поколениями системы и положенными файлами
    # переживает пересборку. Иначе каждая правка кода стоила бы `init-config` → правка → `rebuild`
    # → перезагрузка, то есть трёх загрузок вместо одной.
    start=$(p1_start "$img")
    off=$(( ${start:-0} * 512 ))   # 0 — таблицу разделов прочитать не вышло, образ не наш
    want=$(stat -c%s "$kernel")
    got=""
    if [ "$off" -gt 0 ] && mcopy -o -i "$img@@$off" "$kernel" ::/boot/void-kernel 2>/dev/null; then
        got=$(mdir -i "$img@@$off" ::/boot 2>/dev/null | awk '/void-kernel/ {gsub(/ /,"",$2); print $2}')
    fi
    if [ "$got" = "$want" ]; then
        say "ядро обновлено в образе, store прежних прогонов на месте"
    else
        # Ровно та тихая ошибка, из-за которой в mkdisk.sh появилась проверка (Веха 97): ядро
        # выросло и перестало влезать в загрузочный раздел. Пересобираем — но ГРОМКО, потому
        # что вместе с образом уходит store.
        echo
        echo "  !! ядро не легло в загрузочный раздел образа (влезло '${got:-0}' из $want Б)."
        echo "  !! образ ПЕРЕСОБРАН с нуля — store прежних прогонов ПОТЕРЯН."
        echo
        make_image
    fi
fi

if [ "$build_only" = 1 ]; then
    say "образ готов: $img (QEMU не запускаю — --build-only)"
    exit 0
fi

# ── сценарный прогон ─────────────────────────────────────────────────────────────────────────
#
# KVM здесь не «для скорости», а ради ПРАВДЫ замера: под TCG кадр композитора стоит сотни
# миллисекунд, и любая проверка плавности превращается в проверку эмулятора (см. void-qemu-run).
if [ -n "$script" ]; then
    export VOID_QEMU_MEM="$mem"
    export VOID_QEMU_NET="$net"
    if [ "$tcg" = 0 ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        export VOID_QEMU_ACCEL=kvm
    else
        say "БЕЗ KVM: снимки будут верными, замеры темпа — нет"
    fi
    say "сценарий: $script → $outdir"
    exec python3 "$here/screenrun.py" "$img" "$script" "$outdir"
fi

# ── QEMU ─────────────────────────────────────────────────────────────────────────────────────
qemu=(qemu-system-x86_64 -machine q35 -m "$mem"
      -device ich9-ahci,id=a
      -drive "if=none,id=d,file=$img,format=raw"
      -device ide-hd,drive=d,bus=a.0 -boot c
      -device virtio-rng-pci,disable-legacy=on)

# Без KVM QEMU эмулирует каждую инструкцию: композитор рисует кадр сотни миллисекунд, и любые
# выводы о скорости в таком прогоне — про эмулятор, а не про VOID.
if [ "$tcg" = 0 ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    qemu+=(-accel kvm -cpu host)
else
    [ "$tcg" = 0 ] && say "БЕЗ KVM (/dev/kvm недоступен): всё будет медленным, скорость мерить бессмысленно"
fi

case "$net" in
    none)  qemu+=(-nic none) ;; # без этого QEMU молча добавит карту сам (SLIRP)
    user)  qemu+=(-netdev user,id=net0 -device virtio-net-pci,netdev=net0,disable-legacy=on) ;;
    tap:*) qemu+=(-netdev "tap,id=net0,ifname=${net#tap:},script=no,downscript=no"
                  -device virtio-net-pci,netdev=net0,disable-legacy=on) ;;
    *)     echo "--net: понимаю user, none, tap:<имя> (сегмент двух машин — tools/netlab.py)"; exit 2 ;;
esac

# Писать во временный слой, а не в образ: прогон становится повторяемым (store каждый раз
# стартует с одного поколения) и не мешает второй машине держать тот же файл.
[ "$snapshot" = 1 ] && qemu+=(-snapshot)
[ "$headless" = 1 ] && qemu+=(-display none)
qemu+=(-serial mon:stdio)

echo
say "образ: $img"
if [ "$headless" = 1 ]; then
    say "окна нет (--headless): видно только консоль ядра. Выход: Ctrl-A, затем X"
else
    say "окно QEMU — экран VOID (мышь и клавиатура туда), здесь — консоль ядра"
    say "выход: Ctrl-A, затем X"
fi
echo
exec "${qemu[@]}" "${extra[@]}"
