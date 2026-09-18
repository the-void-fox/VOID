#!/usr/bin/env python3
"""Разбор WAV, записанного QEMU: где звук, какой частоты и не рвался ли он.

Тишиной считается серия нулей длиннее 5 мс, а не одиночный нулевой отсчёт: синус проходит
через ноль дважды за период, и наивный разрез по нулю показывает непрерывный тон как сотню
обрывков по 11 мс. Ровно так этот скрипт и соврал в первый раз.

Размеры в заголовке QEMU дописывает при закрытии файла, а машину мы убиваем, — поэтому
данные читаются от 44-го байта до конца, а не по объявленной длине.
"""
import sys, struct

b = open(sys.argv[1], 'rb').read()
rate = struct.unpack("<I", b[24:28])[0]
s = struct.unpack("<%dh" % ((len(b) - 44) // 2), b[44:])
left = s[0::2]
gap = rate // 200  # 5 мс

runs, start, zeros = [], None, 0
for i, v in enumerate(left):
    if v != 0:
        if start is None:
            start = i
        zeros = 0
    else:
        zeros += 1
        if start is not None and zeros > gap:
            runs.append((start, i - zeros))
            start = None
if start is not None:
    runs.append((start, len(left)))

print(f"{rate} Гц, запись {len(left)/rate:.2f} с, звучащих кусков {len(runs)}")
for a, bb in runs:
    seg = left[a:bb]
    cross = sum(1 for i in range(1, len(seg)) if seg[i-1] < 0 <= seg[i])
    print(f"  с {a/rate*1000:6.0f} мс: {len(seg)/rate*1000:5.0f} мс, {cross/(len(seg)/rate):5.0f} Гц, амплитуда {max(abs(v) for v in seg)}")
