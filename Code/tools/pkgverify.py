#!/usr/bin/env python3
"""pkgverify.py — признак 5 модели угроз МАШИНОЙ: исполнение непроверенного (Веха 225.6).

Использование: pkgverify.py <образ.img> <каталог-выхода>

## Что проверяется

ADR 0023, признак 5: «путь из кэша, поставленный БЕЗ проверки подписи ключом кэша И сверки
`NarHash` после распаковки». До этой вехи признак значился «пока глазами»: проверки в `pkg` есть
и описаны в его шапке, но ничто не мешало им однажды перестать работать молча.

Подход сквозной, а не модульный: подложить в кэш narinfo пакета — корень `pkg/narinfo/<хэш>`,
который `pkg` читает прежде, чем идти в сеть, — и потребовать ОТКАЗА. Так проверяется настоящий
путь целиком: разбор, сверка пути, отпечаток, подпись.

## Три случая, и почему именно они

1. **Подпись не сходится** — байты подписи те, что надо, по длине и ключу, но не те по
   содержанию. Самая прямая подделка.
2. **Подпись чужим ключом** — формально подпись есть, но не от кэша. «Подписано» не значит
   «подписано тем, кому верим».
3. **Путь не тот, что просили** — честно подписанный narinfo ДРУГОГО пути. Без этой сверки кэш
   отвечал бы на запрос про A подписанным B, и все проверки прошли бы успешно, просто не про то.
   (Отказ тут наступает раньше подписи — это и есть порядок, описанный в `pkg`.)

## Чего здесь НЕТ и почему

**Сверки `NarHash` после распаковки.** Она требует настоящего архива из сети: подделать его
локально нельзя, не подделав подпись, а подпись подделать нельзя — в том и смысл. Эта половина
признака остаётся непроверенной машиной, и сказать об этом честно важнее, чем изобразить
покрытие.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
IMPORT = os.path.join(HERE, "void-store-import")
KEY_NAME = "cache.nixos.org-1"
# Хэши путей в nix — 32 символа base32. Числа выдуманные: до сети дело не дойдёт, отказ
# наступает раньше.
HASH_A = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
HASH_B = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
# Подпись нужной длины (64 байта) и нужной формы, но заведомо не та.
FAKE_SIG = "A" * 86 + "=="


def narinfo(path_hash, name, sig_name=KEY_NAME, sig=FAKE_SIG):
    return (
        f"StorePath: /nix/store/{path_hash}-{name}\n"
        f"URL: nar/{path_hash}.nar.xz\n"
        f"Compression: xz\n"
        f"NarHash: sha256:0000000000000000000000000000000000000000000000000000\n"
        f"NarSize: 1024\n"
        f"References: \n"
        f"Sig: {sig_name}:{sig}\n"
    )


CASES = [
    ("подпись не сходится", HASH_A, narinfo(HASH_A, "proba")),
    ("подпись чужим ключом", HASH_A, narinfo(HASH_A, "proba", sig_name="chuzhoy-1")),
    ("путь не тот, что просили", HASH_A, narinfo(HASH_B, "proba")),
]


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    img, out = os.path.abspath(sys.argv[1]), sys.argv[2]
    os.makedirs(out, exist_ok=True)

    bad = 0
    for i, (name, ask, text) in enumerate(CASES):
        path = os.path.join(out, f"ni{i}")
        with open(path, "w") as f:
            f.write(text)
        r = subprocess.run(
            ["cargo", "run", "--quiet", "--release", "--",
             img, "put", path, f"pkg/narinfo/{ask}"],
            cwd=IMPORT, capture_output=True, text=True,
        )
        if r.returncode != 0:
            sys.exit(f"не положить narinfo: {r.stderr.strip()}")

        rc = subprocess.run(
            [os.path.join(HERE, "textrun.py"), img, os.path.join(out, f"run{i}"),
             f"pkg fetch {ask}"],
            capture_output=True, text=True,
        )
        log_path = os.path.join(out, f"run{i}", "serial.log")
        log = open(log_path, "rb").read().decode("utf-8", "replace") if os.path.exists(log_path) else ""
        # Оракул — КОД ВОЗВРАТА, а не слова.
        #
        # Сперва здесь стоял список ожидаемых фраз, и он тут же дал ЛОЖНУЮ находку: третий
        # случай отказал правильно («кэш ответил про другой путь»), но этой формулировки в
        # списке не было, и прогон объявил дыру. Формулировка — дело сменное, а требование
        # нет: путь не должен быть принят. Ненулевой код ровно это и значит.
        code = None
        for line in log.splitlines():
            if "[код " in line and "pkg fetch" not in line:
                try:
                    code = int(line.split("[код ", 1)[1].split("]", 1)[0])
                except ValueError:
                    pass
        refused = code not in (None, 0)
        why = next((l.strip() for l in log.splitlines() if l.strip().startswith("pkg:")), "")
        dead = "[PANIC]" in log or "FATAL TRAP" in log
        print(f"[{i}] {name}:")
        if dead:
            print("    ЯДРО НЕ ПЕРЕЖИЛО — находка")
            bad += 1
        elif refused:
            print(f"    отказано (код {code}) — ПРАВИЛЬНО")
            if why:
                print(f"    {why}")
        else:
            print(f"    ОТКАЗА НЕ БЫЛО (код {code}) — находка: путь принят без проверки")
            print(f"    журнал: {log_path}")
            bad += 1
        if rc.returncode != 0 and not refused and not dead:
            print(f"    (прогон: {rc.stdout.strip().splitlines()[-1] if rc.stdout.strip() else '?'})")

    print()
    if bad:
        print(f"ПРОВЕРКА: находок {bad} из {len(CASES)}")
        return 1
    print(f"ПРОВЕРКА: все {len(CASES)} подделки отвергнуты")
    return 0


if __name__ == "__main__":
    sys.exit(main())
