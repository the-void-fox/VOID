#!/usr/bin/env bash
# stage-drivers.sh — собрать C-драйверы (портированный код Linux) и выложить их туда, откуда
# их заберёт сборка ядра (Веха 132).
#
# Зачем. Программы на Rust ядро несёт в себе и сеет в store на загрузке. C-драйверы собирает
# nix, и раньше они попадали в систему мостом `void-store-import` — то есть правкой ОБРАЗА.
# На ноутбуке этого мало: store там на внутреннем SATA-диске, а грузимся мы с флешки, и USB
# для VOID вообще не блочное устройство. Единственный способ доставить драйвер на такую
# машину — вложить его в ядро.
#
#   Code/tools/stage-drivers.sh          обе архитектуры
#   Code/tools/stage-drivers.sh x86_64   только одна
#
# После этого обычная сборка ядра включит драйверы семенами. Без этого шага ядро соберётся
# БЕЗ них и скажет об этом — и при сборке (cargo:warning), и на загрузке ([seed]).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

# Драйвер → атрибут деривации в nix/default.nix.
declare -A DRIVERS=( [lx-atl1c-hw]=lx_atl1c_drv [lx-atl1c-full]=lx_atl1c_full )

arches=("${@:-x86_64 riscv64}")
read -ra arches <<< "${arches[*]}"

for arch in "${arches[@]}"; do
    dest="$repo/Code/programs/lx-linux/prebuilt/$arch"
    mkdir -p "$dest"
    for drv in "${!DRIVERS[@]}"; do
        attr="${DRIVERS[$drv]}"
        echo "  собираю $drv для $arch…"
        out=$(cd "$repo" && nix-build nix -A "$arch.$attr" --no-out-link)
        install -m 0644 "$out/bin/$drv" "$dest/$drv"
        echo "  выложен $dest/$drv ($(stat -c%s "$dest/$drv") Б)"
    done
done
