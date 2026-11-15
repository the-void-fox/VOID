#!/usr/bin/env python3
r"""Принять таблицу ACPI, снятую с живой машины VOID, и сразу её дизассемблировать (Веха 234).

Зачем. Батарея, крышка и сон описаны не в нашем коде, а в прошивке конкретной машины — байт-кодом
AML. Исполнять его VOID пока не умеет, а решать, писать ли свой разбор или брать чужой
(ACPICA), надо ПО ЭТОМУ КОДУ: сколько там методов, какие опкоды, как глубоко идут вызовы. Снять
таблицу с машины и разобрать её на хосте — единственный честный способ это узнать.

Как пользоваться. На хосте:

    python3 Code/tools/acpi-recv.py 9000 log/acpi

В VOID на той машине (адрес хоста — его адрес в сети, в QEMU это 10.0.2.2):

    acpi DSDT >> send "10.0.2.2" 9000

Скрипт собирает текст, проверяет длину по заголовку самой таблицы, кладёт `DSDT.aml` и, если в
окружении есть `iasl`, тут же кладёт рядом `DSDT.dsl` — читаемый текст. Удобный способ получить
`iasl`, не ставя ничего в систему:

    nix-shell -p acpica-tools --run "python3 Code/tools/acpi-recv.py 9000 log/acpi"

Разбор методов потом: `acpiexec DSDT.aml` умеет ВЫПОЛНИТЬ метод на хосте (`execute _SB.BAT0._BST`)
— это и есть оракул, с которым можно сверять свой разбор, когда он появится.
"""
import os
import re
import shutil
import socket
import subprocess
import sys

port = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
outdir = sys.argv[2] if len(sys.argv) > 2 else "."
os.makedirs(outdir, exist_ok=True)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", port))
srv.listen(1)
print(f"жду таблицу на порту {port} (в VOID: acpi DSDT >> send \"адрес-хоста\" {port})")

def receive_one():
    conn, addr = srv.accept()
    chunks = []
    while True:
        b = conn.recv(65536)
        if not b:
            break
        chunks.append(b)
    conn.close()
    text = b"".join(chunks).decode("utf-8", "replace")
    print(f"\nпринято {len(text)} знаков от {addr[0]}")
    return text

# Таблиц присылают НЕСКОЛЬКО подряд (DSDT, обе SSDT, ECDT), поэтому слушаем в цикле, пока не
# прервут Ctrl+C: иначе человеку пришлось бы перезапускать скрипт между командами и помнить,
# какая по счёту уже пришла.
while True:
    try:
        text = receive_one()
    except KeyboardInterrupt:
        print("\nготово")
        break

    # Первая строка — заголовок команды: «# DSDT 70000 байт (в заголовке 70000)».
    name = "TABLE"
    claimed = None
    head = re.match(r"#\s+(\w{4})\s+(\d+)\s+байт\s+\(в заголовке\s+(\d+)\)", text)
    if head:
        name, _, claimed = head.group(1), int(head.group(2)), int(head.group(3))

    data = bytearray()
    for line in text.splitlines():
        m = re.match(r"^[0-9a-f]{6}\s+([0-9a-f]+)$", line.strip())
        if m:
            data += bytes.fromhex(m.group(1))

    if not data:
        print("в принятом тексте нет шестнадцатеричных строк — не та команда?")
        continue

    # Проверка по самой таблице: сигнатура и длина лежат в её заголовке. Без неё молча принятый
    # обрезок выглядел бы как успех — и дизассемблер выдал бы правдоподобный мусор.
    sig = bytes(data[:4]).decode("ascii", "replace")
    length = int.from_bytes(data[4:8], "little")
    ok = len(data) == length and (claimed is None or claimed == length)
    print(f"сигнатура {sig}, длина по заголовку {length}, получено {len(data)} — "
          f"{'сходится' if ok else 'НЕ СХОДИТСЯ'}")
    if not ok:
        print("таблица неполная: увеличь буфер в команде или проверь, что весь вывод дошёл")

    aml = os.path.join(outdir, f"{sig if sig.isalnum() else name}.aml")
    with open(aml, "wb") as f:
        f.write(data)
    print(f"записано: {aml}")

    if shutil.which("iasl"):
        # `-d` дизассемблирует; рядом ляжет .dsl. Код возврата у iasl бывает ненулевым и при
        # успешном разборе (предупреждения), поэтому смотрим на файл, а не на код.
        subprocess.run(["iasl", "-d", aml], cwd=outdir or ".", check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        dsl = aml[:-4] + ".dsl"
        if os.path.exists(dsl):
            lines = sum(1 for _ in open(dsl, errors="replace"))
            print(f"разобрано: {dsl} ({lines} строк)")
        else:
            print("iasl есть, но .dsl не появился — разбирай руками: iasl -d", aml)
    else:
        print("iasl не найден: nix-shell -p acpica-tools --run \"iasl -d " + aml + "\"")
