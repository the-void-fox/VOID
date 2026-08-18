#!/usr/bin/env python3
r"""shot.py — ЧИСЛА по снимкам экрана VOID: где край окна, что за пиксель, что изменилось.

Зачем инструмент. Снимки из `screenrun.py` — единственный способ увидеть графику VOID в
разработке, но глазами по ним проверяют только «похоже/не похоже». Разница между «анимация
доехала» и «анимация промахнулась на восемь пикселей» глазами не видна вовсе, а именно она и
была последними тремя ошибками (Вехи 145.1–145.3). Поэтому здесь — измерение, а не просмотр.

Почему свой декодер PNG, а не Pillow: **PIL в этом окружении нет**, а тянуть его в nix-shell
ради четырёх команд — менять одну зависимость на другую. PNG из QEMU всегда 8 бит на канал,
без чересстрочности, так что разбор умещается в полсотни строк.

    Code/tools/shot.py box    кадр.png                 рамка всего, что не фон
    Code/tools/shot.py edge   кадр.png --x 200         верх/низ непустого в столбце x
    Code/tools/shot.py px     кадр.png 640 400         цвет пикселя
    Code/tools/shot.py row    кадр.png 18              полосы цветов вдоль строки
    Code/tools/shot.py col    кадр.png 640             то же вдоль столбца
    Code/tools/shot.py crop   кадр.png 0 0 320 64 в.png
    Code/tools/shot.py diff   а.png б.png              сколько пикселей разошлось и где

Общие ключи: `--bg RRGGBB` (иначе фон = самый частый цвет кадра), `--tol N` (допуск, 24).

Ловушка, на которой уже обожглись: **фон VOID не чёрный.** Обои — градиент, и порог «ярче
двадцати» ловил их сами по себе. Поэтому фон здесь не константа, а цвет, которого в кадре
больше всего; всё, что отличается от него сильнее допуска, считается содержимым.
"""
import struct
import sys
import zlib
from collections import Counter


def read_png(path):
    """→ (ширина, высота, байт-на-пиксель, [строки байтами]). Только 8 бит/канал, без Adam7."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: это не PNG")
    idat, w, h, depth, ctype, i = b"", 0, 0, 8, 6, 8
    while i < len(data):
        (ln,) = struct.unpack(">I", data[i : i + 4])
        tag, chunk = data[i + 4 : i + 8], data[i + 8 : i + 8 + ln]
        if tag == b"IHDR":
            w, h, depth, ctype, _, _, lace = struct.unpack(">IIBBBBB", chunk[:13])
            if depth != 8 or lace or ctype not in (2, 6):
                sys.exit(f"{path}: поддержаны только 8-битные RGB/RGBA без чересстрочности")
        elif tag == b"IDAT":
            idat += chunk
        elif tag == b"IEND":
            break
        i += 12 + ln
    bpp = 4 if ctype == 6 else 3
    raw, stride, rows, prev, o = zlib.decompress(idat), w * bpp, [], bytearray(w * bpp), 0
    for _ in range(h):
        f = raw[o]
        o += 1
        line = bytearray(raw[o : o + stride])
        o += stride
        # Фильтры PNG — по спецификации; Paeth разворачивать вручную быстрее, чем объяснять.
        if f == 1:
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 255
        elif f == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 255
        elif f == 3:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 255
        elif f == 4:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                c = prev[x - bpp] if x >= bpp else 0
                b = prev[x]
                p, pa, pb, pc = a + b - c, abs(b - c), abs(a - c), abs(a + b - 2 * c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(line))
        prev = line
    return w, h, bpp, rows


def write_png(path, w, h, bpp, rows):
    raw = b"".join(b"\x00" + r for r in rows)
    ctype = 6 if bpp == 4 else 2

    def chunk(tag, body):
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body))

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, ctype, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def pixel(rows, bpp, x, y):
    r = rows[y]
    return tuple(r[x * bpp : x * bpp + 3])


def background(rows, bpp, w, h, given):
    """Фон — самый частый цвет кадра. Считаем по сетке: полный проход по миллиону пикселей
    нужен только ради ответа «какого цвета тут больше всего», а он не меняется от прореживания."""
    if given:
        return tuple(int(given[i : i + 2], 16) for i in (0, 2, 4))
    c = Counter()
    for y in range(0, h, max(1, h // 200)):
        for x in range(0, w, max(1, w // 200)):
            c[pixel(rows, bpp, x, y)] += 1
    return c.most_common(1)[0][0]


def near(a, b, tol):
    return abs(a[0] - b[0]) <= tol and abs(a[1] - b[1]) <= tol and abs(a[2] - b[2]) <= tol


def hexc(c):
    return "%02x%02x%02x" % c


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        sys.exit(__doc__)
    opt = {"bg": None, "tol": 24, "x": None}
    rest = []
    i = 0
    while i < len(args):
        if args[i] == "--bg":
            opt["bg"] = args[i + 1].lstrip("#")
            i += 2
        elif args[i] == "--tol":
            opt["tol"] = int(args[i + 1])
            i += 2
        elif args[i] == "--x":
            opt["x"] = int(args[i + 1])
            i += 2
        else:
            rest.append(args[i])
            i += 1
    cmd, rest = rest[0], rest[1:]
    tol = opt["tol"]

    if cmd == "diff":
        w, h, bpp, a = read_png(rest[0])
        w2, h2, bpp2, b = read_png(rest[1])
        if (w, h) != (w2, h2):
            sys.exit(f"размеры разные: {w}x{h} и {w2}x{h2}")
        n, x0, y0, x1, y1 = 0, w, h, -1, -1
        for y in range(h):
            if a[y] == b[y]:
                continue
            for x in range(w):
                if not near(pixel(a, bpp, x, y), pixel(b, bpp2, x, y), tol):
                    n += 1
                    x0, y0, x1, y1 = min(x0, x), min(y0, y), max(x1, x), max(y1, y)
        if n == 0:
            print("кадры совпадают (допуск %d)" % tol)
        else:
            print(
                "разошлось %d пикселей (%.2f%%), рамка x %d..%d, y %d..%d (%dx%d)"
                % (n, 100.0 * n / (w * h), x0, x1, y0, y1, x1 - x0 + 1, y1 - y0 + 1)
            )
        return

    w, h, bpp, rows = read_png(rest[0])

    if cmd == "px":
        x, y = int(rest[1]), int(rest[2])
        c = pixel(rows, bpp, x, y)
        a = rows[y][x * bpp + 3] if bpp == 4 else 255
        print("(%d,%d) = #%s  rgb%s  a=%d" % (x, y, hexc(c), c, a))
        return

    if cmd == "crop":
        x, y, cw, ch = (int(v) for v in rest[1:5])
        out = rest[5]
        cut = [r[(x + 0) * bpp : (x + cw) * bpp] for r in rows[y : y + ch]]
        write_png(out, cw, ch, bpp, cut)
        print("%s: %dx%d из (%d,%d)" % (out, cw, ch, x, y))
        return

    bg = background(rows, bpp, w, h, opt["bg"])

    if cmd == "box":
        x0, y0, x1, y1 = w, h, -1, -1
        for y in range(h):
            for x in range(w):
                if not near(pixel(rows, bpp, x, y), bg, tol):
                    x0, y0, x1, y1 = min(x0, x), min(y0, y), max(x1, x), max(y1, y)
        if x1 < 0:
            print("фон #%s — кадр пуст целиком" % hexc(bg))
        else:
            print(
                "фон #%s · содержимое x %d..%d, y %d..%d (%dx%d)"
                % (hexc(bg), x0, x1, y0, y1, x1 - x0 + 1, y1 - y0 + 1)
            )
        return

    if cmd == "edge":
        x = opt["x"] if opt["x"] is not None else w // 6
        ys = [y for y in range(h) if not near(pixel(rows, bpp, x, y), bg, tol)]
        if not ys:
            print("столбец x=%d: фон #%s по всей высоте" % (x, hexc(bg)))
        else:
            print(
                "столбец x=%d (фон #%s): верх %d, низ %d, высота %d"
                % (x, hexc(bg), ys[0], ys[-1], ys[-1] - ys[0] + 1)
            )
        return

    if cmd in ("row", "col"):
        n = int(rest[1])
        span = w if cmd == "row" else h
        seq = [pixel(rows, bpp, i, n) if cmd == "row" else pixel(rows, bpp, n, i) for i in range(span)]
        # Полосами, а не пикселями: у панели и капсулы важны ГРАНИЦЫ, а печатать тысячу
        # значений — прятать ответ в шуме.
        out, start = [], 0
        for i in range(1, span + 1):
            if i == span or not near(seq[i], seq[start], tol):
                if i - start >= 2:
                    out.append("%d..%d #%s" % (start, i - 1, hexc(seq[start])))
                start = i
        print(("строка y=%d" if cmd == "row" else "столбец x=%d") % n)
        for s in out:
            print("  " + s)
        return

    sys.exit("не знаю команды %r (--help)" % cmd)


main()
