#!/usr/bin/env python3
r"""redagent.py — АГЕНТ красной команды (Веха 152.1): модель без исходников тычется в VOID.

Второй шаг после оракула ([[redteam]]): дать языковой модели РУКИ (серийный shell VOID) и ГЛАЗА
(его вывод), и пусть строит гипотезы — как добыть власть, которой не давали, или уронить сервис.
Ценность по-прежнему в ОРАКУЛЕ, а не в «уме» модели: пробой ловит харнесс (усиление прав, паника,
зависание), а модель лишь порождает попытки, до которых структурный фаззер не додумается.

Устройство:
  • ТРАНСПОРТ — серийная консоль. VOID грузится в текстовый `vsh` (поколение с УРЕЗАННЫМ набором
    прав — принципал-атакующий), а агент пишет команды в serial и читает ответ до приглашения.
  • МОЗГ — сменный:
      --replay <файл>   сценарный мозг: команды из файла, по одной на строку (проверка транспорта
                        и оракула БЕЗ модели);
      иначе             LLM через OpenAI-совместимый endpoint (ollama/koboldcpp). Модель, хост и
                        порт — из окружения (REDAGENT_MODEL/REDAGENT_URL), задача — в системном
                        промпте ниже.
  • ОРАКУЛ — в потоке serial: `PROBE VERDICT AMPLIFICATION`, `[PANIC]`, `FATAL TRAP`; плюс
    зависание (приглашение не вернулось). Находка останавливает прогон и печатается.

Транскрипт (что модель говорила, что VOID отвечал) пишется в <вых>/transcript.txt — это
воспроизводимость: находку можно повторить и без модели, тем же `--replay`.

Запуск:
    redagent.py --replay attack.txt        сценарный прогон (без модели)
    redagent.py                            LLM-прогон (нужен запущенный ollama/koboldcpp)
    REDAGENT_MODEL=mistral-nemo redagent.py --turns 40

Окружение: REDAGENT_URL (по умолч. http://localhost:11434/v1/chat/completions — ollama),
REDAGENT_MODEL, REDAGENT_TURNS, плюс стенд VOID_QEMU_* (как у screenrun/run.sh).
"""
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
CODE = os.path.dirname(HERE)
MACHINE_SH = os.path.join(HERE, "qemu-machine.sh")
IMG = os.environ.get("VOID_IMG", os.path.join(CODE, "target", "void.img"))

# ── стенд из общего описания (как в screenrun.py) ──────────────────────────────────────────
def stand(*args):
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


# ── признаки пробоя (те же, что у redteam.py) ──────────────────────────────────────────────
FINDING_RX = [
    (re.compile(r"PROBE VERDICT AMPLIFICATION.*"), "усиление-прав"),
    (re.compile(r"\[PANIC\].*"), "паника-ядра"),
    (re.compile(r"FATAL TRAP.*"), "фатальный-трап"),
]
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# Приглашение оболочки: «vsh…> » или «vvsh…> ». ИЩЕМ вхождение, а не конец потока: ядро и
# net-srv досыпают асинхронные строки ПОСЛЕ приглашения (DHCP, самопинг), и якорь на конец
# принимал вернувшееся приглашение за зависание. Цвета к этому месту уже сняты.
PROMPT_RX = re.compile(r"v{1,2}sh/[^\n]*?>\s")

SYSTEM_PROMPT = """\
Ты — исследователь безопасности. Перед тобой ОС без исходников (микроядро на capability-модели):
у процесса нет прав по умолчанию, всё — по непередаваемым ссылкам (capability). Ты сидишь в её
командной оболочке `vsh` с УРЕЗАННЫМ набором прав. Твоя цель — ДОБЫТЬ власть, которой тебе не
давали (например право выключить машину, доступ к сети, запись в хранилище), ЛИБО уронить сервис.

Правила ответа, СТРОГО:
- отвечай РОВНО одной строкой — командой оболочки, без пояснений, без кавычек, без markdown;
- одна команда за ход; результат ты увидишь и решишь, что дальше;
- есть `help` (список команд), `roots` (объекты хранилища), `run <имя>` (запустить программу).
  Программа `probe` печатает, до скольких прав ты реально дотягиваешься против выданных, — это
  твой измеритель конфайнмента;
- если считаешь, что дыра найдена, ответь одной строкой: DONE <короткое описание>.

Не рассуждай вслух. Только команда."""


class Serial:
    """Двусторонняя серийная консоль QEMU: пишем команды, читаем вывод, следим за оракулом."""

    def __init__(self, outdir):
        self.buf = bytearray()
        self.lock = threading.Lock()
        self.log = open(os.path.join(outdir, "serial.log"), "wb")
        accel = os.environ.get("VOID_QEMU_ACCEL", "")
        cpu = os.environ.get("VOID_QEMU_CPU", "")
        mem = os.environ.get("VOID_QEMU_MEM", "")
        net = os.environ.get("VOID_QEMU_NET", "user")
        qemu = [
            "qemu-system-x86_64",
            *stand("machine", IMG, mem),
            *(["-accel", accel] if accel else []),
            *(["-cpu", cpu] if cpu else []),
            *stand("net", net, os.environ.get("VOID_QEMU_NIC", ""),
                   os.environ.get("VOID_QEMU_MAC", ""), os.environ.get("VOID_QEMU_PCAP", ""),
                   os.environ.get("VOID_QEMU_DELAY_US", "")),
            "-display", "none",
            "-serial", "stdio",
        ]
        self.p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, bufsize=0)
        self.reader = threading.Thread(target=self._pump, daemon=True)
        self.reader.start()

    def _pump(self):
        while True:
            b = self.p.stdout.read(1)
            if not b:
                return
            self.log.write(b)
            self.log.flush()
            with self.lock:
                self.buf.extend(b)

    def _text(self):
        with self.lock:
            return ANSI.sub("", self.buf.decode("utf-8", "replace"))

    def finding(self):
        """Первый признак пробоя во всём, что видели. `None` — чисто."""
        txt = self._text()
        for rx, name in FINDING_RX:
            m = rx.search(txt)
            if m:
                return (name, m.group(0).strip())
        return None

    def wait_prompt(self, pos, timeout):
        """Ждать НОВОГО приглашения в потоке, начиная с байта `pos` (мет­ка перед командой).

        Ищем вхождение приглашения ПОСЛЕ нашей команды, а не в самом хвосте: асинхронные строки
        ядра/сети приходят после него, и требовать приглашение последним значило бы принять живую
        систему за зависшую. `False` — таймаут (зависание) или смерть QEMU (паника/выключение)."""
        end = time.time() + timeout
        while time.time() < end:
            if self.p.poll() is not None:
                return False  # QEMU умер (паника/выключение)
            if PROMPT_RX.search(self.since(pos)):
                return True
            time.sleep(0.2)
        return False

    def send(self, line):
        """Команду — ПОБАЙТНО с паузой (Веха 138: FIFO 16550 глотает пачку под KVM)."""
        for ch in (line + "\n").encode():
            try:
                self.p.stdin.write(bytes([ch]))
                self.p.stdin.flush()
            except (BrokenPipeError, OSError):
                return
            time.sleep(0.02)

    def mark(self):
        """Запомнить длину буфера — чтобы вырезать ответ РОВНО на эту команду."""
        with self.lock:
            return len(self.buf)

    def since(self, pos):
        with self.lock:
            return ANSI.sub("", self.buf[pos:].decode("utf-8", "replace"))

    def close(self):
        try:
            self.p.terminate()
        except OSError:
            pass


# ── МОЗГИ ──────────────────────────────────────────────────────────────────────────────────
class ReplayBrain:
    """Сценарный мозг: команды из файла, по одной на строку (# — комментарий)."""

    def __init__(self, path):
        self.cmds = []
        for line in open(path, encoding="utf-8"):
            s = line.strip()
            if s and not s.startswith("#"):
                self.cmds.append(s)
        self.i = 0

    def next(self, _transcript):
        if self.i >= len(self.cmds):
            return None
        c = self.cmds[self.i]
        self.i += 1
        return c


class LlmBrain:
    """LLM через OpenAI-совместимый endpoint (ollama/koboldcpp)."""

    def __init__(self):
        self.url = os.environ.get("REDAGENT_URL", "http://localhost:11434/v1/chat/completions")
        self.model = os.environ.get("REDAGENT_MODEL", "mistral-nemo")
        self.history = [{"role": "system", "content": SYSTEM_PROMPT}]

    def next(self, transcript):
        # Последний ответ VOID кладём как наблюдение; на первом ходу — затравка.
        obs = transcript[-1][1] if transcript else "Ты в оболочке vsh. Действуй."
        self.history.append({"role": "user", "content": f"Вывод VOID:\n{obs}\n\nСледующая команда:"})
        body = json.dumps({
            "model": self.model,
            "messages": self.history,
            "temperature": 0.7,
            "max_tokens": 64,
            "stream": False,
        }).encode()
        req = urllib.request.Request(self.url, body, {"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                reply = json.loads(r.read())["choices"][0]["message"]["content"]
        except Exception as e:  # noqa: BLE001 — модель/сеть чего угодно
            print(f"[redagent] LLM недоступен ({e}); проверь ollama/koboldcpp на {self.url}")
            return None
        self.history.append({"role": "assistant", "content": reply})
        # Берём ПЕРВУЮ непустую строку как команду — модель могла добавить лишнего.
        for raw in reply.strip().splitlines():
            cmd = raw.strip().strip("`").strip()
            if cmd:
                return cmd
        return None


def main():
    args = sys.argv[1:]
    replay = None
    turns = int(os.environ.get("REDAGENT_TURNS", "30"))
    outdir = None
    i = 0
    while i < len(args):
        if args[i] == "--replay":
            replay = args[i + 1]; i += 2
        elif args[i] == "--turns":
            turns = int(args[i + 1]); i += 2
        elif args[i] == "--out":
            outdir = args[i + 1]; i += 2
        else:
            print(__doc__); return 2
    if outdir is None:
        import tempfile
        outdir = tempfile.mkdtemp(prefix="void-redagent-")
    os.makedirs(outdir, exist_ok=True)

    if not os.path.exists(IMG):
        sys.exit(f"нет образа {IMG} — собери: Code/tools/run.sh --fresh --build-only")

    brain = ReplayBrain(replay) if replay else LlmBrain()
    print(f"[redagent] мозг: {'replay ' + replay if replay else 'LLM ' + brain.model}")
    print(f"[redagent] образ: {IMG} → {outdir}")

    ser = Serial(outdir)
    transcript = []
    tr = open(os.path.join(outdir, "transcript.txt"), "w", encoding="utf-8")

    def log(s):
        print(s)
        tr.write(s + "\n")
        tr.flush()

    # Дождаться загрузки — первого приглашения оболочки.
    log("[redagent] жду загрузки VOID до приглашения оболочки…")
    if not ser.wait_prompt(0, 90):
        f = ser.finding()
        log(f"[redagent] приглашение не пришло за 90 с; оракул: {f}")
        ser.close()
        return 1

    finding = None
    for turn in range(1, turns + 1):
        cmd = brain.next(transcript)
        if cmd is None:
            log(f"[redagent] мозг больше не даёт команд (ход {turn})")
            break
        if cmd.upper().startswith("DONE"):
            log(f"[redagent] модель объявила: {cmd}")
            break
        log(f"\n─── ход {turn} ─── > {cmd}")
        pos = ser.mark()
        ser.send(cmd)
        ok = ser.wait_prompt(pos, 30)
        out = ser.since(pos)
        log(out.rstrip("\n"))
        transcript.append((cmd, out))

        finding = ser.finding()
        if finding:
            log(f"\n*** НАХОДКА [{finding[0]}]: {finding[1]} ***")
            break
        if not ok:
            finding = ("зависание", f"после «{cmd}» приглашение не вернулось (30 с)")
            log(f"\n*** НАХОДКА [{finding[0]}]: {finding[1]} ***")
            break

    ser.close()
    tr.close()
    print("\n──────── СВОДКА ────────")
    print(f"ходов        : {len(transcript)}")
    print(f"транскрипт   : {os.path.join(outdir, 'transcript.txt')}")
    if finding:
        print(f"находка      : [{finding[0]}] {finding[1]}")
        print("────────────────────────")
        return 1
    print("находка      : нет")
    print("────────────────────────")
    return 0


if __name__ == "__main__":
    sys.exit(main())
