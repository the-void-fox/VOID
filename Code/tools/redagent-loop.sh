#!/usr/bin/env bash
# redagent-loop.sh — НОЧНОЙ цикл красной команды: гоняет redagent.py эпизод за эпизодом,
# после исчерпания лимита ходов пишет итог в мастер-лог и НАЧИНАЕТ ЗАНОВО. Останавливается на
# РЕАЛЬНОЙ находке (эскалация/паника/зависание) либо по общему дедлайну.
#
# Эксперимент «может ли локальная 12B за ночь что-то найти» ([[redteam]]). Ценность по-прежнему
# в оракуле; это лишь долгий прогон свободного мозга с подправленным промптом и памятью попыток.
#
# БЕЗОПАСНОСТЬ ОБРАЗА: рабочий target/void.img НЕ трогаем. В начале делаем изолированную копию
# (scratch.img) и весь цикл гоним через VOID_IMG по ней. Твой сеанс/поколение целы.
#
# Запуск ТОЛЬКО из nix-shell (qemu на PATH):
#   nix-shell Code/shell.nix --run 'Code/tools/redagent-loop.sh'
#
# Окружение (всё с разумными умолчаниями):
#   LOOP_HOURS      сколько часов крутить             (по умолч. 12)
#   LOOP_EPISODES   предел эпизодов, 0 = без предела  (по умолч. 0)
#   REDAGENT_TURNS  ходов на эпизод                   (по умолч. 40)
#   REDAGENT_MODEL  модель                            (по умолч. mistral-nemo)
#   REDAGENT_URL    endpoint OpenAI-совместимого API  (по умолч. ollama :11434)
#   REDAGENT_REPLAY если задан — сценарный мозг из файла (для дымового теста БЕЗ модели)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CODE="$(dirname "$HERE")"
PRISTINE="$CODE/target/void.img"
WORKDIR="$CODE/target/redagent-loop"
SCRATCH="$WORKDIR/scratch.img"
MASTER="$WORKDIR/master.log"

LOOP_HOURS="${LOOP_HOURS:-12}"
LOOP_EPISODES="${LOOP_EPISODES:-0}"
export REDAGENT_TURNS="${REDAGENT_TURNS:-40}"
export REDAGENT_MODEL="${REDAGENT_MODEL:-mistral-nemo}"
export REDAGENT_URL="${REDAGENT_URL:-http://localhost:11434/v1/chat/completions}"

if [ ! -f "$PRISTINE" ]; then
  echo "нет образа $PRISTINE — собери сперва: Code/tools/run.sh --fresh --build-only" >&2
  exit 1
fi
if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
  echo "нет qemu-system-x86_64 в PATH — запусти через nix-shell Code/shell.nix --run '…'" >&2
  exit 1
fi

# Преполёт мозга: если endpoint недоступен ИЛИ модель не грузится, вся ночь уйдёт в пустые эпизоды —
# лучше упасть сразу. Заодно ПРОГРЕВАЕМ модель: первый запрос холодно грузит веса в VRAM (для 12B —
# десятки секунд, что рвало таймаут внутри эпизода), делаем это ОДИН раз здесь с длинным таймаутом.
# Сценарный мозг (--replay) преполёта не требует.
if [ -z "${REDAGENT_REPLAY:-}" ]; then
  echo "[redagent-loop] прогрев модели $REDAGENT_MODEL (холодная загрузка в VRAM может занять минуту)…"
  python3 - "$REDAGENT_URL" "$REDAGENT_MODEL" <<'PY' || exit 1
import sys, json, time, urllib.request
url, model = sys.argv[1], sys.argv[2]
body = json.dumps({"model": model,
                   "messages": [{"role": "user", "content": "ok"}],
                   "max_tokens": 1, "stream": False}).encode()
req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
t0 = time.time()
try:
    with urllib.request.urlopen(req, timeout=600) as r:
        json.loads(r.read())
except Exception as e:
    sys.exit(f"[redagent-loop] модель не прогрелась за 600 с ({e}) — проверь ollama/модель '{model}'")
print(f"[redagent-loop] модель прогрета за {time.time() - t0:.0f} с")
PY
fi

mkdir -p "$WORKDIR"
echo "[redagent-loop] изолирую образ: $PRISTINE → $SCRATCH"
cp -f "$PRISTINE" "$SCRATCH"
export VOID_IMG="$SCRATCH"

deadline=$(( $(date +%s) + LOOP_HOURS * 3600 ))
started="$(date '+%Y-%m-%d %H:%M:%S')"
if [ -n "${REDAGENT_REPLAY:-}" ]; then
  brain_desc="replay ${REDAGENT_REPLAY}"
else
  brain_desc="$REDAGENT_MODEL @ $REDAGENT_URL"
fi
{
  echo "════════════════════════════════════════════════════════════"
  echo "старт          : $started"
  echo "мозг           : $brain_desc"
  echo "ходов/эпизод   : $REDAGENT_TURNS"
  echo "предел         : ${LOOP_HOURS}ч, эпизодов $([ "$LOOP_EPISODES" = 0 ] && echo '∞' || echo "$LOOP_EPISODES")"
  echo "образ (копия)  : $SCRATCH"
  echo "════════════════════════════════════════════════════════════"
} | tee -a "$MASTER"

ep=0
bootfail=0
found=""
while :; do
  now=$(date +%s)
  if [ "$now" -ge "$deadline" ]; then
    echo "[redagent-loop] дедлайн ${LOOP_HOURS}ч — стоп" | tee -a "$MASTER"; break
  fi
  if [ "$LOOP_EPISODES" != 0 ] && [ "$ep" -ge "$LOOP_EPISODES" ]; then
    echo "[redagent-loop] предел эпизодов ($LOOP_EPISODES) — стоп" | tee -a "$MASTER"; break
  fi
  ep=$((ep + 1))
  epdir="$WORKDIR/ep-$(printf '%04d' "$ep")"
  mkdir -p "$epdir"
  ts="$(date '+%H:%M:%S')"

  set +e
  python3 "$HERE/redagent.py" \
    ${REDAGENT_REPLAY:+--replay "$REDAGENT_REPLAY"} \
    --turns "$REDAGENT_TURNS" --out "$epdir" >"$epdir/episode.out" 2>&1
  set -e

  tr="$epdir/transcript.txt"
  turns_done=$(grep -c '^─── ход ' "$tr" 2>/dev/null || true)
  if grep -q '\*\*\* НАХОДКА' "$tr" 2>/dev/null; then
    line=$(grep -m1 '\*\*\* НАХОДКА' "$tr" | sed 's/^[[:space:]]*//')
    echo "[$ts] эп $ep · ходов $turns_done · $line" | tee -a "$MASTER"
    echo "" | tee -a "$MASTER"
    echo "██ НАХОДКА в эпизоде $ep — цикл остановлен. Транскрипт: $tr" | tee -a "$MASTER"
    found="$epdir"
    break
  fi

  if grep -q 'приглашение не пришло' "$tr" 2>/dev/null; then
    bootfail=$((bootfail + 1))
    echo "[$ts] эп $ep · ЗАГРУЗКА НЕ ПОДНЯЛАСЬ (подряд $bootfail) — пересоздаю образ" | tee -a "$MASTER"
    if [ "$bootfail" -ge 3 ]; then
      echo "[redagent-loop] 3 сбоя загрузки подряд — что-то со стендом, стоп" | tee -a "$MASTER"; break
    fi
    cp -f "$PRISTINE" "$SCRATCH"   # вдруг образ заклинило накопленным состоянием
    continue
  fi
  bootfail=0
  echo "[$ts] эп $ep · ходов $turns_done · находок нет" | tee -a "$MASTER"
done

{
  echo "────────────────────────────────────────────────────────────"
  echo "финиш          : $(date '+%Y-%m-%d %H:%M:%S')  (старт: $started)"
  echo "эпизодов        : $ep"
  if [ -n "$found" ]; then
    echo "ИТОГ           : НАХОДКА → $found/transcript.txt"
  else
    echo "ИТОГ           : находок нет"
  fi
  echo "мастер-лог      : $MASTER"
  echo "════════════════════════════════════════════════════════════"
} | tee -a "$MASTER"

[ -z "$found" ]
