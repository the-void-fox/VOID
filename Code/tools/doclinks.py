#!/usr/bin/env python3
r"""doclinks.py — проверить ССЫЛКИ ИЗ КОДА В ДОКУМЕНТАЦИЮ (Веха 214.7).

Комментарии VOID ссылаются на заметки и ADR записью `[[имя]]` — так в коде оказывается не
пересказ решения, а указатель на него. Ссылок таких две сотни, и это хорошо: проза не
дублируется, а адресуется.

Плохо другое: **никто их не проверял**. Заметку переименовали — ссылка осталась; заметку не
написали вовсе — ссылка всё равно стоит. Первый прогон этой проверки нашёл 30 битых из 218, то
есть каждую седьмую. Хуже того, семь имён вели в ЛИЧНУЮ ПАМЯТЬ ассистента — в документы,
которых в репозитории нет и не будет: читатель VOID видел ссылку и не находил ничего.

Ссылка, ведущая в никуда, — это проза, которую нельзя проверить. Ровно то, от чего проект
отказался правилом «код → комментарий → заметка» ([[notes-conventions]]): у документации есть
порядок доверия, и висящая ссылка выбивает из него нижнюю ступень.

    python3 Code/tools/doclinks.py          проверить, код выхода = число битых
    python3 Code/tools/doclinks.py --list   заодно показать все живые ссылки

Вендоренные крейты и портированный код Linux не проверяются: там чужие комментарии.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CODE = ROOT / "Code"
DOCS = ROOT / "Obsidian" / "10-projects" / "void"
SKIP = ("vendor", "lx-linux", "target")


def notes():
    """Имена всех заметок и ADR (без расширения)."""
    return {p.stem for p in DOCS.rglob("*.md")}


def links():
    """[(имя, файл, строка)] всех `[[…]]` из КОММЕНТАРИЕВ кода."""
    out = []
    for p in sorted(CODE.rglob("*.rs")):
        if any(s in str(p) for s in SKIP):
            continue
        for n, line in enumerate(p.read_text(errors="ignore").splitlines(), 1):
            if not line.strip().startswith("//"):
                continue
            # `[[имя|подпись]]` и `[[имя#якорь]]` — цель это то, что до `|` и `#`
            for m in re.findall(r"\[\[([^\]]+)\]\]", line):
                out.append((m.split("|")[0].split("#")[0].strip(), p, n))
    return out


def main():
    known = notes()
    all_links = links()
    broken = [(t, p, n) for t, p, n in all_links if t not in known]

    if "--list" in sys.argv:
        for t, p, n in all_links:
            mark = "×" if t not in known else " "
            print(f" {mark} {p.relative_to(ROOT)}:{n}  [[{t}]]")
        print()

    print(f"ссылок в коде : {len(all_links)} на {len({t for t, _, _ in all_links})} имён")
    print(f"заметок и ADR : {len(known)}")
    if not broken:
        print("битых         : нет — каждая ссылка ведёт в существующий документ")
        return 0

    print(f"БИТЫХ         : {len(broken)}\n")
    for t in sorted({t for t, _, _ in broken}):
        places = [f"{p.relative_to(ROOT)}:{n}" for tt, p, n in broken if tt == t]
        print(f"  [[{t}]]  ({len(places)})")
        for pl in places:
            print(f"      {pl}")
    return len(broken)


if __name__ == "__main__":
    sys.exit(main())
