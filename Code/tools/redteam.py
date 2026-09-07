#!/usr/bin/env python3
r"""redteam.py — ХАРНЕСС красной команды (Веха 152): гоняет зонды по VOID и судит по ОРАКУЛУ.

Пользователь предложил натравить на систему агента без исходников — искать неявные дыры за долгий
горизонт. Первый шаг, выбранный сознательно: НЕ агент, а оракул и харнесс. Без чёткого признака
пробоя любой агент утонет в шуме собственного вывода; а с ним даже тупой перебор уже ловит
падения сервисов и утечки прав.

Что это делает:
  1. собирает образ и гоняет сценарий через `run.sh --script` (тот же screenrun, тот же стенд);
  2. читает serial-лог и судит по трём признакам:
       • УСИЛЕНИЕ ПРАВ  — строка `PROBE VERDICT AMPLIFICATION` от зонда конфайнмента (`probe`):
                          процесс дотянулся до права, которого ему не выдавали;
       • ПАДЕНИЕ ЯДРА   — `[PANIC]` или `FATAL TRAP`: сервис или всё ядро легли;
       • ЗАВИСАНИЕ      — зонд запущен, но вердикта в логе нет: гость завис или умер молча;
  3. печатает сводку и выходит с кодом = числу находок (0 — чисто).

Оракул зонда безвреден: живость права он выясняет копией-аттенуацией (`SYS_CAP_DERIVE`), цель не
трогается. Поэтому кампанию можно крутить долго и часто — это, по сути, фаззинг-CI: перезапускай
после каждой правки протокола, и он поймает регрессию в том, что тронули.

Использование:
    redteam.py                       прогон со встроенным сценарием (probe ×2)
    redteam.py <сценарий.txt>        свой сценарий (строки — как у screenrun.py)
    redteam.py <сценарий.txt> <вых>  ещё и каталог выхода

Стенд, ускоритель и память — те же переменные, что у run.sh/screenrun.py (VOID_QEMU_ACCEL и т.д.).
Число ядер — `VOID_QEMU_SMP` (Веха 170): конфайнмент проверялся на 1, 4 и 8 — вердикты совпадают.

Образ у харнесса СВОЙ (`target/void-redteam.img`) и стирается каждым прогоном: `--fresh` иначе
снёс бы рабочий образ владельца. Переопределяется `VOID_IMG`.
"""
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
RUN = os.path.join(HERE, "run.sh")

# Веха 170 — СВОЙ образ, а не рабочий.
#
# Ниже стоит `--fresh`, и это обязательно: зонд `probe` сеется в store из ядра, а красной команде
# нужен чистый стенд на каждый прогон. Но `--fresh` СТИРАЕТ образ, а `run.sh` по умолчанию берёт
# рабочий — тот, в котором лежит поколение, набранное руками, и сохранённый сеанс. Прогон
# харнесса не должен стоить владельцу его системы. Переопределить — `VOID_IMG`.
IMG = os.environ.get("VOID_IMG") or os.path.join(HERE, "..", "target", "void-redteam.img")

# Встроенный сценарий: дважды запустить зонд в ОДНУ загрузку. Первый прогон — свежий домен
# «probe» (ожидаем CONFINED); второй — тот же домен ПЕРЕИСПОЛЬЗУЕТСЯ, и если c-space копит права
# от прошлого процесса, второй зонд это и покажет. Ровно тот случай, ради которого харнесс написан.
# Терминал открывается ЯВНО (Веха 172): с тех пор как «что открыть при входе» переехало в конфиг
# (`autostart`), система больше не открывает окно терминала сама — по умолчанию там окно
# «добро пожаловать». Сценарий, печатавший сразу, попадал буквами в него, зонды не запускались, и
# харнесс честно рапортовал «запущено 2, вердиктов 0 — кто-то не ответил». Ошибка была не в
# конфайнменте, а в том, что проверка держала в уме прошлое умолчание.
DEFAULT_SCENARIO = """\
sleep 18
hotkey Super+Return
sleep 4
type probe
hotkey Return
sleep 4
type probe
hotkey Return
sleep 4
shot 01-probe
"""

# Признаки пробоя в serial-логе. Каждый — (регэксп, короткое имя класса).
FINDINGS = [
    # Веха 152.2 — находка = ЭСКАЛАЦИЯ (власть сверх выданной). DUP (лишние слоты той же власти)
    # тревогой НЕ считается — это накопление c-space, а не пробой.
    (re.compile(r"PROBE VERDICT ESCALATION.*"), "усиление-прав"),
    (re.compile(r"\[PANIC\].*"), "паника-ядра"),
    (re.compile(r"FATAL TRAP.*"), "фатальный-трап"),
]

# Сколько зондов сценарий запускает (строк `type probe`) — по нему судим о зависании: запущено N,
# а вердиктов меньше → кто-то завис или умер молча.
def count_probes(scenario_text):
    n = 0
    for line in scenario_text.splitlines():
        s = line.strip()
        if s == "type probe" or s.startswith("type probe "):
            n += 1
    return n


def main():
    scenario_path = sys.argv[1] if len(sys.argv) > 1 else None
    outdir = sys.argv[2] if len(sys.argv) > 2 else tempfile.mkdtemp(prefix="void-redteam-")

    if scenario_path:
        scenario_text = open(scenario_path, encoding="utf-8").read()
        scen = scenario_path
        tmp = None
    else:
        tmp = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8")
        tmp.write(DEFAULT_SCENARIO)
        tmp.flush()
        scenario_text = DEFAULT_SCENARIO
        scen = tmp.name

    os.makedirs(outdir, exist_ok=True)
    print(f"[redteam] сценарий {scen} → {outdir}")

    # `--fresh`: зонд (`probe`) сеется в store из ядра, а обычный образ несёт store прошлых
    # прогонов без него. Красной команде свежий store и нужен — чистый стенд на каждый прогон.
    cmd = [RUN, "--fresh", "--img", IMG, "--script", scen, outdir]
    print(f"[redteam] образ {IMG} (свой, стирается каждым прогоном)")
    subprocess.run(cmd, check=False)

    serial = os.path.join(outdir, "serial.log")
    if not os.path.exists(serial):
        print("[redteam] НЕТ serial.log — прогон не поднялся; считаю это находкой")
        return 1
    log = open(serial, "rb").read().decode("utf-8", "replace")

    findings = []
    for rx, name in FINDINGS:
        for m in rx.finditer(log):
            findings.append((name, m.group(0).strip()))

    launched = count_probes(scenario_text)
    verdicts = len(re.findall(r"PROBE VERDICT", log))
    if launched and verdicts < launched:
        findings.append(
            ("зависание", f"запущено зондов {launched}, вердиктов {verdicts} — кто-то не ответил")
        )

    print("\n──────── СВОДКА ────────")
    confined = len(re.findall(r"PROBE VERDICT CONFINED", log))
    print(f"зондов запущено : {launched}")
    print(f"CONFINED        : {confined}")
    if not findings:
        print("находок         : нет — конфайнмент держит, ядро живо")
        print("────────────────────────")
        return 0
    print(f"находок         : {len(findings)}")
    for name, line in findings:
        print(f"  • [{name}] {line}")
    print("────────────────────────")
    return len(findings)


if __name__ == "__main__":
    sys.exit(main())
