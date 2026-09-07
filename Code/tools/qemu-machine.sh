#!/usr/bin/env bash
# qemu-machine.sh — СТЕНД VOID одним описанием: машина, диск, память, энтропия и сеть.
#
# ── Зачем отдельный файл ─────────────────────────────────────────────────────────────────────
#
# Командную строку QEMU строили ДВА места: `run.sh` (обычный запуск, окно) и `screenrun.py`
# (сценарный прогон — клавиши, мышь, снимки, замеры). Списки устройств в них совпадали только
# потому, что их держали в голове: чипсет, контроллер диска, `-boot`, virtio-rng и сетевая карта
# были написаны дважды, а память вообще расходилась (1280 МиБ против 512).
#
# Цена расхождения здесь выше обычной. Замер, сделанный на другой машине, не значит ничего, а
# отличие стенда не видно в глаза: гость поднимается, снимки получаются, числа выходят —
# только они про другую машину (см. notes/void-qemu-run.md и «QEMU прячет ошибки»). Добавить
# устройство в одном месте и забыть про второе — это ровно та тихая ошибка, которую мы уже
# ловили с ядром, собранным до правки.
#
# ── Что здесь есть и чего здесь нет ──────────────────────────────────────────────────────────
#
# Здесь СТЕНД: из чего машина состоит. Всё, чем прогоны честно ОТЛИЧАЮТСЯ, остаётся у них:
# окно против `-display none`, `mon:stdio` против `stdio`+QMP, `-snapshot`, политика ускорителя
# (сценарный прогон по умолчанию идёт на TCG — снимкам скорость не нужна, а `run.sh` берёт KVM,
# когда `/dev/kvm` доступен). Сводить это вместе было бы уже не описанием стенда, а попыткой
# сделать два разных инструмента одним.
#
# ── Использование ────────────────────────────────────────────────────────────────────────────
#
#   source qemu-machine.sh                → функции void_qemu_machine / void_qemu_net
#   qemu-machine.sh machine <образ> [память]
#   qemu-machine.sh net [сеть] [карта] [mac] [pcap] [задержка-мкс]
#
# Печатает аргументы ПО ОДНОМУ НА СТРОКУ: внутри них есть и пробелы, и запятые, и разбирать
# вывод обратно словами — это ждать беды. Пустой аргумент = «возьми умолчание» (в том числе из
# окружения), поэтому вызывающему не нужно повторять здешние умолчания у себя.
#
# `set -e` здесь НЕ ставится сознательно: файл подключают через `source` в чужой скрипт, и менять
# ему режим оболочки — не наше дело.

# Память стенда. Одна на оба прогона: 512 МиБ и 1280 МиБ — это РАЗНЫЕ машины, и найденное на
# одной может не воспроизвестись на другой (куча ядра, кэш store в ней же, потолок числа окон).
VOID_QEMU_MEM_DEFAULT=1280M

# Машина: чипсет, память, диск с образом и источник энтропии.
#
# Диск идёт через AHCI (`ich9-ahci` + `ide-hd`), а не virtio-blk, потому что грузимся мы GRUB'ом
# с образа, и это ближе всего к тому, как VOID стартует на настоящей машине владельца.
void_qemu_machine() {
    local img="$1"
    local mem="${2:-}"
    if [ -z "$img" ]; then
        echo "стенд: не сказан образ (qemu-machine.sh machine <образ> [память])" >&2
        return 2
    fi
    if [ -z "$mem" ]; then
        mem="${VOID_QEMU_MEM:-$VOID_QEMU_MEM_DEFAULT}"
    fi
    # Веха 170 — СКОЛЬКО ЯДЕР у стенда. Умолчание — одно: пока система работает на одном ядре,
    # менять стенд значило бы обесценить все прежние замеры (они делались на одноядерной
    # машине). Многоядерные прогоны просят это явно: `VOID_QEMU_SMP=4`.
    local smp="${VOID_QEMU_SMP:-1}"
    printf '%s\n' \
        -machine q35 \
        -smp "$smp" \
        -m "$mem" \
        -device ich9-ahci,id=a \
        -drive "if=none,id=d,file=$img,format=raw" \
        -device ide-hd,drive=d,bus=a.0 \
        -boot c \
        -device virtio-rng-pci,disable-legacy=on
}

# Сетевой стенд (Веха 135).
#
#   сеть    user | seg:<путь> | join:<путь> | tap:<имя> | none
#   карта   virtio | e1000 | none
#   mac     пусто — умолчание QEMU (одинаковое у всех машин! на общем сегменте так нельзя)
#   pcap    файл — записать ВСЁ, что прошло через карту
#   мкс     задержка канала: локально RTT ~0.3 мс, и на таком проводе не видно ничего, что
#           зависит от произведения «полоса × задержка»
#
# `user` — это SLIRP, то есть НАТ внутри QEMU, а не провод: отвечает мгновенно и всегда, сам
# сочиняет ответы на ARP и DHCP, широковещание за свои пределы не пускает. Для сетевой фазы
# стенд — `seg:`/`join:` (два VOID'а кадр в кадр, tools/netlab.py).
void_qemu_net() {
    local net="${1:-}" nic="${2:-}" mac="${3:-}" pcap="${4:-}" delay="${5:-}"
    [ -n "$net" ] || net="${VOID_QEMU_NET:-user}"
    [ -n "$nic" ] || nic="${VOID_QEMU_NIC:-virtio}"
    [ -n "$mac" ] || mac="${VOID_QEMU_MAC:-}"
    [ -n "$pcap" ] || pcap="${VOID_QEMU_PCAP:-}"
    [ -n "$delay" ] || delay="${VOID_QEMU_DELAY_US:-}"

    # `-nic none` обязателен: без КАКИХ-ЛИБО сетевых ключей QEMU молча добавит карту сам (SLIRP).
    # Проверено — «none» давал гостю сеть и адрес по DHCP, то есть ровно то, что просили выключить.
    if [ "$net" = none ] || [ "$nic" = none ]; then
        printf '%s\n' -nic none
        return 0
    fi

    local kind="${net%%:*}" arg=""
    if [ "$kind" != "$net" ]; then
        arg="${net#*:}"
    fi
    local backend
    case "$kind" in
        user) backend="user,id=net0" ;;
        # Сегмент L2 между двумя VOID'ами: первый слушает unix-сокет, второй подключается.
        # Привилегий не требует, чужого стека посередине нет.
        seg)  backend="stream,id=net0,server=on,addr.type=unix,addr.path=$arg" ;;
        join) backend="stream,id=net0,server=off,addr.type=unix,addr.path=$arg" ;;
        # script=no/downscript=no: поднимать tap — дело хозяина стенда, не QEMU.
        tap)  backend="tap,id=net0,ifname=$arg,script=no,downscript=no" ;;
        *)
            echo "стенд: не понимаю сеть '$net' (user | seg:<путь> | join:<путь> | tap:<имя> | none)" >&2
            return 2
            ;;
    esac

    local dev
    case "$nic" in
        virtio) dev="virtio-net-pci,netdev=net0,disable-legacy=on" ;;
        e1000)  dev="e1000,netdev=net0" ;;
        *)
            echo "стенд: не понимаю карту '$nic' (virtio | e1000 | none)" >&2
            return 2
            ;;
    esac
    if [ -n "$mac" ]; then
        dev="$dev,mac=$mac"
    fi

    printf '%s\n' -netdev "$backend" -device "$dev"
    if [ -n "$pcap" ]; then
        printf '%s\n' -object "filter-dump,id=dump0,netdev=net0,file=$pcap"
    fi
    if [ -n "$delay" ]; then
        printf '%s\n' -object "filter-buffer,id=lag0,netdev=net0,interval=$delay"
    fi
    return 0
}

# Позвали как программу (а не `source`) — отдать запрошенный кусок стенда.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    case "${1:-}" in
        machine) shift; void_qemu_machine "$@" ;;
        net)     shift; void_qemu_net "$@" ;;
        *)
            echo "qemu-machine.sh machine <образ> [память] | net [сеть] [карта] [mac] [pcap] [мкс]" >&2
            exit 2
            ;;
    esac
fi
