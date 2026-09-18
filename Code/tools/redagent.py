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

# Мост host→store: правит поколение атакующего в образе, пока QEMU не запущен. `musl` ПЕРВЫМ —
# `gnu`-сборка бывает устаревшей и читает актуальный store как пустой.
STORE_IMPORT = next(
    (p for p in (
        os.path.join(HERE, "void-store-import", "target", a, "release", "void-store-import")
        for a in ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"))
     if os.path.exists(p)),
    None,
)

# Поколение принципала-атакующего: серийный vsh с УРЕЗАННЫМ набором прав. Без power и сети —
# чтобы «модель выключила машину ВЫДАННЫМ правом» не считалось за пробой; `store:rx` — читать и
# запускать программы, но НЕ писать. Ставится в system/redteam, current → redteam (см. setup).
REDTEAM_GEN = (
    "# VOID — красная команда: УРЕЗАННЫЙ принципал (без power/сети, store r+x)\n"
    "service posixfs store:rw\n"
    "shell vsh endpoint:posixfs store:rx env\n"
)


def store(*args):
    """Одна команда моста к store образа. `None` — моста нет; иначе (код, stdout)."""
    if STORE_IMPORT is None:
        return None
    r = subprocess.run([STORE_IMPORT, IMG, *args], capture_output=True, text=True)
    return (r.returncode, r.stdout)

# ── стенд из общего описания (как в screenrun.py) ──────────────────────────────────────────
def stand(*args):
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


# ── признаки пробоя (те же, что у redteam.py) ──────────────────────────────────────────────
FINDING_RX = [
    # Веха 152.2 — находка = ЭСКАЛАЦИЯ (власть сверх выданной); DUP не тревога (накопление).
    (re.compile(r"PROBE VERDICT ESCALATION.*"), "усиление-прав"),
    (re.compile(r"\[PANIC\].*"), "паника-ядра"),
    (re.compile(r"FATAL TRAP.*"), "фатальный-трап"),
]
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# Приглашение оболочки: «vsh…> » или «vvsh…> ». ИЩЕМ вхождение, а не конец потока: ядро и
# net-srv досыпают асинхронные строки ПОСЛЕ приглашения (DHCP, самопинг), и якорь на конец
# принимал вернувшееся приглашение за зависание. Цвета к этому месту уже сняты.
PROMPT_RX = re.compile(r"v{1,2}sh/[^\n]*?>\s")
# Полноэкранная интерактивная программа (`ved`) ПО ЗАМЫСЛУ держит терминал и не отдаёт приглашение,
# пока из неё не выйдешь. Это НЕ зависание ядра — узнаём редактор по его экрану и выходим `^Q`, иначе
# штатный редактор считается за пробой (ложные находки, вскрыл ночной прогон 2026-08-25). Признаков
# несколько: у нового/пустого файла ved рисует «новый файл: …» ВМЕСТО легенды `^Q выход`, поэтому
# одной легенды мало — ловим ещё заливку пустых строк `~~~~`, статус-строку `1:1  N строк` и «новый
# файл:». Все они специфичны для ved и в обычном выводе оболочки не встречаются.
INTERACTIVE_RX = re.compile(r"~{6,}|\^Q\s*выход|новый файл:|\d+:\d+\s+\d+\s+строк")

SYSTEM_PROMPT = """\
Ты — исследователь безопасности. Перед тобой ОС БЕЗ ИСХОДНИКОВ: микроядро на capability-модели.
Это НЕ Linux и НЕ POSIX. Нет пользователей, нет root, нет /proc, нет команды `id`, нет путей вроде
/etc. У процесса нет прав по умолчанию — всё только по непередаваемым ссылкам (capability). Ты
сидишь в оболочке vvsh с УРЕЗАННЫМ набором прав: хранилище только чтение+запуск (store r+x, БЕЗ
записи), без права выключить машину, без сети.

Команды оболочки (других НЕТ — не выдумывай Linux-команды):
  ls [DIR] · cat FILE · tail FILE · cd [DIR] · pwd · mkdir DIR · rm PATH · mv OLD NEW ·
  echo TEXT [> FILE] · ved FILE · grep SUB · run NAME · thaw NAME · ping IP · resolve NAME ·
  tcp-connect · fetch · blob · unroot · roots · rebuild · gens · switch GEN · sysdef · date ·
  random · log on|off · run probe · poweroff · exit
`roots` (объекты хранилища) и `help` — разведка, нужны ОДИН раз; повторять их бессмысленно.
Файл читай через `cat`/`tail`, НЕ через `ved`: `ved` — полноэкранный редактор, он захватывает
терминал и не вернёт приглашение, пока не выйдешь `^Q`.

ЦЕЛЬ: получить власть, которой тебе НЕ давали (запись в хранилище, право выключить машину, сеть),
либо уронить сервис/ядро.

ИЗМЕРИТЕЛЬ — `run probe`. Он печатает один из вердиктов:
  CONFINED    — ты заперт, дотянулся ровно до выданного;
  DUP +N      — ты лишь ПРОДУБЛИРОВАЛ власть, что у тебя и так есть: это НЕ дыра и НЕ прогресс,
                гонять probe снова ради роста N бесполезно;
  ESCALATION  — ты дотянулся до власти СВЕРХ выданной — ВОТ это находка.
Паника ядра или зависание системы — тоже находка.

ДЕЙСТВУЙ ОСОЗНАННО: сам по себе probe власти не даёт. Чтобы её изменить, надо изменить СОСТОЯНИЕ
системы, а затем ОДИН раз замерить probe. Например: разморозить чужой процесс (`thaw ИМЯ`),
переключить поколение (`switch GEN`), пересобрать (`rebuild`), попытаться записать в корень
хранилища. Меняешь что-то → `run probe` один раз → если снова CONFINED/DUP, этот путь не сработал,
пробуй ДРУГОЙ. Строй гипотезу, не тычь наугад и не повторяй уже виденное.

Формат ответа, СТРОГО: РОВНО одна строка — команда оболочки, без пояснений, кавычек и markdown.
Если уверен, что нашёл дыру: DONE <короткое описание>."""


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

    def send_raw(self, data):
        """Сырые байты — ПОБАЙТНО с паузой (Веха 138: FIFO 16550 глотает пачку под KVM)."""
        for ch in data:
            try:
                self.p.stdin.write(bytes([ch]))
                self.p.stdin.flush()
            except (BrokenPipeError, OSError):
                return
            time.sleep(0.02)

    def send(self, line):
        """Команду со завершающим переводом строки."""
        self.send_raw((line + "\n").encode())

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
        # Таймаут HTTP: первый запрос может ХОЛОДНО грузить веса в VRAM (12B — десятки секунд),
        # 120 с не хватало. Настраиваемо; ночной цикл вдобавок прогревает модель до эпизодов.
        self.timeout = int(os.environ.get("REDAGENT_HTTP_TIMEOUT", "300"))
        self.history = [{"role": "system", "content": SYSTEM_PROMPT}]
        self.tried = []       # уникальные поданные команды, по порядку
        self.seen = set()

    def next(self, transcript):
        # Последний ответ VOID кладём как наблюдение; на первом ходу — затравка. Плюс компактный
        # список уже поданных команд с явным «не повторяй» — иначе 12B схлопывается в цикл
        # roots→help→run probe (см. [[redteam]]: первый прогон дал ровно это).
        obs = transcript[-1][1] if transcript else "Ты в оболочке VOID. Действуй."
        tried = ", ".join(self.tried[-24:]) if self.tried else "(пока ничего)"
        self.history.append({"role": "user", "content":
            f"Вывод VOID на прошлую команду:\n{obs}\n\n"
            f"Ты уже подавал в этом сеансе: {tried}\n"
            f"Не повторяй команду, чей вывод уже известен и ничего не поменялось — пробуй новое.\n"
            f"Одна новая команда:"})
        body = json.dumps({
            "model": self.model,
            "messages": self.history,
            "temperature": 0.8,
            "max_tokens": 64,
            "stream": False,
        }).encode()
        req = urllib.request.Request(self.url, body, {"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                reply = json.loads(r.read())["choices"][0]["message"]["content"]
        except Exception as e:  # noqa: BLE001 — модель/сеть чего угодно
            print(f"[redagent] LLM недоступен ({e}); проверь ollama/koboldcpp на {self.url}")
            return None
        self.history.append({"role": "assistant", "content": reply})
        # Берём ПЕРВУЮ непустую строку как команду — модель могла добавить лишнего.
        for raw in reply.strip().splitlines():
            cmd = raw.strip().strip("`").strip()
            if cmd:
                if cmd not in self.seen:
                    self.seen.add(cmd)
                    self.tried.append(cmd)
                return cmd
        return None


def main():
    args = sys.argv[1:]
    replay = None
    turns = int(os.environ.get("REDAGENT_TURNS", "30"))
    outdir = None
    setup_gen = True
    i = 0
    while i < len(args):
        if args[i] == "--replay":
            replay = args[i + 1]; i += 2
        elif args[i] == "--turns":
            turns = int(args[i + 1]); i += 2
        elif args[i] == "--out":
            outdir = args[i + 1]; i += 2
        elif args[i] == "--no-setup":
            # Не трогать поколение — гость уже грузится в серийный shell (сам поставил current).
            setup_gen = False; i += 1
        else:
            print(__doc__); return 2
    if outdir is None:
        import tempfile
        outdir = tempfile.mkdtemp(prefix="void-redagent-")
    os.makedirs(outdir, exist_ok=True)

    if not os.path.exists(IMG):
        sys.exit(f"нет образа {IMG} — собери: Code/tools/run.sh --fresh --build-only")

    # ── ПОКОЛЕНИЕ АТАКУЮЩЕГО. Агенту нужен СЕРИЙНЫЙ shell (в оконном режиме приглашения на serial
    #    нет вовсе — shell живёт в окне). Ставим урезанное серийное поколение сами и возвращаем
    #    прежнее после прогона: рабочий образ не должен застревать в режиме красной команды.
    saved_gen = None
    if setup_gen:
        if STORE_IMPORT is None:
            print("[redagent] нет void-store-import — не поставить серийное поколение; "
                  "либо собери мост, либо переведи образ на серийный shell и добавь --no-setup")
        else:
            cur = store("cat", "system/current")
            out = cur[1] if cur else ""
            if "store не инициализирован" in out or "пустой образ" in out:
                sys.exit("[redagent] store пуст — сперва загрузи образ разок "
                         "(Code/tools/run.sh), потом запускай агента")
            saved_gen = out.strip().splitlines()[-1].strip() if out.strip() else None
            # Записать поколение атакующего (идемпотентно) и сделать активным.
            import tempfile
            gt = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8")
            gt.write(REDTEAM_GEN); gt.flush()
            ct = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8")
            ct.write("redteam"); ct.flush()
            store("put", gt.name, "system/redteam")
            store("put", ct.name, "system/current")
            print(f"[redagent] поколение атакующего активно (было: {saved_gen or '?'})")

    def restore_gen():
        if saved_gen and STORE_IMPORT is not None:
            import tempfile
            ct = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8")
            ct.write(saved_gen); ct.flush()
            store("put", ct.name, "system/current")
            print(f"[redagent] поколение образа возвращено на: {saved_gen}")

    brain = ReplayBrain(replay) if replay else LlmBrain()
    print(f"[redagent] мозг: {'replay ' + replay if replay else 'LLM ' + brain.model}")
    print(f"[redagent] образ: {IMG} → {outdir}")

    ser = Serial(outdir)
    transcript = []
    finding = None
    tr = open(os.path.join(outdir, "transcript.txt"), "w", encoding="utf-8")

    def log(s):
        print(s)
        tr.write(s + "\n")
        tr.flush()

    try:
        # Дождаться загрузки — первого приглашения оболочки.
        log("[redagent] жду загрузки VOID до приглашения оболочки…")
        if not ser.wait_prompt(0, 90):
            f = ser.finding()
            log(f"[redagent] приглашение не пришло за 90 с; оракул: {f}")
            log("[redagent] подсказка: агент ждёт СЕРИЙНЫЙ shell (vsh). Если образ грузится в "
                "оконный режим — это оно; поколение атакующего ставится автоматически, проверь мост "
                "void-store-import.")
            return 1

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
                # Полноэкранный редактор (ved) держит терминал ПО ЗАМЫСЛУ — не зависание. Узнаём
                # по легенде клавиш, выходим ^Q и продолжаем; штатная программа не должна быть «пробоем».
                if INTERACTIVE_RX.search(out):
                    log("[redagent] полноэкранная программа (ved) держит терминал — выход по ^Q, это не зависание")
                    qpos = ser.mark()
                    ser.send_raw(b"\x11")  # Ctrl-Q — выход из ved
                    if ser.wait_prompt(qpos, 12):
                        transcript[-1] = (cmd, out + "\n[redagent] (полноэкранный редактор — вышел по ^Q)")
                        continue
                finding = ("зависание", f"после «{cmd}» приглашение не вернулось (30 с)")
                log(f"\n*** НАХОДКА [{finding[0]}]: {finding[1]} ***")
                break
    finally:
        ser.close()
        tr.close()
        restore_gen()  # рабочий образ не должен застрять в режиме красной команды

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
