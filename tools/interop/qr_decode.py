#!/usr/bin/env python3
"""QR コードの PNG を読む小さな復号器（試験用、標準ライブラリだけ）。

senca-sdk の QR 生成（Rust）とは別に書いた実装で、生成した画像を読み戻して中身を確かめる。
対象: 誤り訂正 M、型番 1〜10、数字・バイトモード（1 セグメント）、白黒 1 bit または 8 bit グレースケールの PNG。
画像の構造（位置検出パターン・タイミング・形式情報の 2 つの写しが一致し BCH 符号として正しいこと）と、
Reed-Solomon のシンドロームが 0 であることも確かめる。

使い方: qr_decode.py <in.png>   → 中身を標準出力へ。壊れていれば終了コード 1。
"""
import struct
import sys
import zlib

# 型番ごとの (総コード語数, ブロックあたりの EC 語数, [(ブロック数, データ語数), ...])（レベル M）
BLOCKS_M = {
    1: (26, 10, [(1, 16)]), 2: (44, 16, [(1, 28)]), 3: (70, 26, [(1, 44)]), 4: (100, 18, [(2, 32)]),
    5: (134, 24, [(2, 43)]), 6: (172, 16, [(4, 27)]), 7: (196, 18, [(4, 31)]),
    8: (242, 22, [(2, 38), (2, 39)]), 9: (292, 22, [(3, 36), (2, 37)]), 10: (346, 26, [(4, 43), (1, 44)]),
}
ALIGN = {1: [], 2: [6, 18], 3: [6, 22], 4: [6, 26], 5: [6, 30], 6: [6, 34], 7: [6, 22, 38],
         8: [6, 24, 42], 9: [6, 26, 46], 10: [6, 28, 50]}


def fail(msg):
    print("qr_decode: " + msg, file=sys.stderr)
    sys.exit(1)


def read_png(path):
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        fail("not a PNG")
    pos, idat, w = 8, b"", None
    while pos < len(data):
        n, = struct.unpack(">I", data[pos:pos + 4])
        kind, body = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + n]
        crc, = struct.unpack(">I", data[pos + 8 + n:pos + 12 + n])
        if zlib.crc32(kind + body) & 0xFFFFFFFF != crc:
            fail("bad chunk CRC")
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if ctype != 0 or depth not in (1, 8) or interlace:
                fail("unsupported PNG format")
        elif kind == b"IDAT":
            idat += body
        pos += 12 + n
    raw = zlib.decompress(idat)
    stride = (w * depth + 7) // 8
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        bpp = 1
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b, c = prev[i], prev[i - bpp] if i >= bpp else 0
            if f == 1: line[i] = (line[i] + a) & 255
            elif f == 2: line[i] = (line[i] + b) & 255
            elif f == 3: line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c; pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        prev = line
        if depth == 1:
            rows.append([not (line[x // 8] >> (7 - x % 8)) & 1 for x in range(w)])
        else:
            rows.append([line[x] < 128 for x in range(w)])
    return w, h, rows


def modules(w, h, px):
    # 左上の位置検出パターン: 最初の暗い画素から、外枠の 7 モジュール分の横幅で 1 モジュールの画素数を求める
    for y0 in range(h):
        if any(px[y0]):
            break
    x0 = px[y0].index(True)
    run = 0
    while x0 + run < w and px[y0][x0 + run]:
        run += 1
    scale = run / 7
    n = round((w - 2 * x0) / scale)
    if (n - 17) % 4 or not 21 <= n <= 57:
        fail("unexpected symbol size %d" % n)
    get = lambda x, y: px[int(y0 + (y + 0.5) * scale)][int(x0 + (x + 0.5) * scale)]
    return n, [[get(x, y) for x in range(n)] for y in range(n)]


def bch_format(d):
    r = d << 10
    for i in range(14, 9, -1):
        if r >> i & 1:
            r ^= 0x537 << (i - 10)
    return ((d << 10) | r) ^ 0x5412


def gf_mul(a, b):
    r = 0
    while b:
        if b & 1: r ^= a
        a <<= 1
        if a & 0x100: a ^= 0x11D
        b >>= 1
    return r


def main():
    w, h, px = read_png(sys.argv[1])
    n, m = modules(w, h, px)
    v = (n - 17) // 4
    # 位置検出パターン
    for cx, cy in ((3, 3), (n - 4, 3), (3, n - 4)):
        for dy in range(-3, 4):
            for dx in range(-3, 4):
                d = max(abs(dx), abs(dy))
                if m[cy + dy][cx + dx] != (d != 2):
                    fail("finder pattern broken")
    # タイミングパターン
    for i in range(8, n - 8):
        if m[6][i] != (i % 2 == 0) or m[i][6] != (i % 2 == 0):
            fail("timing pattern broken")
    if not m[n - 8][8]:
        fail("dark module missing")
    # 形式情報の 2 つの写し
    a = b = 0
    for i in range(6): a |= m[i][8] << i
    a |= m[7][8] << 6 | m[8][8] << 7 | m[8][7] << 8
    for i in range(9, 15): a |= m[8][14 - i] << i
    for i in range(8): b |= m[8][n - 1 - i] << i
    for i in range(8, 15): b |= m[n - 15 + i][8] << i
    if a != b:
        fail("format copies differ")
    fmt = [d for d in range(32) if bch_format(d) == a]
    if not fmt:
        fail("format info is not a valid BCH codeword")
    ecl, mask = fmt[0] >> 3, fmt[0] & 7
    if ecl != 0:
        fail("not error correction level M")
    # 機能パターンの場所
    func = [[False] * n for _ in range(n)]
    def mark(x0, y0, x1, y1):
        for y in range(max(0, y0), min(n, y1)):
            for x in range(max(0, x0), min(n, x1)):
                func[y][x] = True
    mark(0, 0, 9, 9); mark(n - 8, 0, n, 9); mark(0, n - 8, 9, n)
    mark(6, 0, 7, n); mark(0, 6, n, 7)
    al = ALIGN[v]
    for i, ay in enumerate(al):
        for j, ax in enumerate(al):
            if (i, j) in ((0, 0), (0, len(al) - 1), (len(al) - 1, 0)):
                continue
            mark(ax - 2, ay - 2, ax + 3, ay + 3)
    if v >= 7:
        mark(n - 11, 0, n - 8, 6); mark(0, n - 11, 6, n - 8)
    masks = [lambda x, y: (x + y) % 2 == 0, lambda x, y: y % 2 == 0, lambda x, y: x % 3 == 0,
             lambda x, y: (x + y) % 3 == 0, lambda x, y: (x // 3 + y // 2) % 2 == 0,
             lambda x, y: x * y % 2 + x * y % 3 == 0, lambda x, y: (x * y % 2 + x * y % 3) % 2 == 0,
             lambda x, y: ((x + y) % 2 + x * y % 3) % 2 == 0]
    bits = []
    right = n - 1
    while right >= 1:
        if right == 6:
            right = 5
        for vert in range(n):
            for j in range(2):
                x = right - j
                y = n - 1 - vert if ((right + 1) & 2) == 0 else vert
                if not func[y][x]:
                    bits.append(m[y][x] ^ masks[mask](x, y))
        right -= 2
    total, ec, groups = BLOCKS_M[v]
    cw = [int("".join("1" if b else "0" for b in bits[i * 8:i * 8 + 8]), 2) for i in range(total)]
    lens = [l for c, l in groups for _ in range(c)]
    blocks = [[] for _ in lens]
    k = 0
    for i in range(max(lens)):
        for bi, l in enumerate(lens):
            if i < l:
                blocks[bi].append(cw[k]); k += 1
    for _ in range(ec):
        for bl in blocks:
            bl.append(cw[k]); k += 1
    data = []
    for bl, l in zip(blocks, lens):
        root = 1
        for _ in range(ec):
            s = 0
            for c in bl:
                s = gf_mul(s, root) ^ c
            if s:
                fail("Reed-Solomon syndrome is not zero")
            root = gf_mul(root, 2)
        data += bl[:l]
    stream = "".join(format(c, "08b") for c in data)
    mode = stream[:4]
    if mode == "0100":
        cb = 8 if v <= 9 else 16
        cnt = int(stream[4:4 + cb], 2)
        out = bytes(int(stream[4 + cb + 8 * i:12 + cb + 8 * i], 2) for i in range(cnt))
        sys.stdout.write(out.decode("utf-8"))
    elif mode == "0001":
        cb = 10 if v <= 9 else 12
        cnt, pos, out = int(stream[4:4 + cb], 2), 4 + cb, ""
        while cnt:
            g = min(cnt, 3); nb = {1: 4, 2: 7, 3: 10}[g]
            out += str(int(stream[pos:pos + nb], 2)).zfill(g); pos += nb; cnt -= g
        sys.stdout.write(out)
    else:
        fail("unsupported mode " + mode)


if __name__ == "__main__":
    main()
