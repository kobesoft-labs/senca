#!/usr/bin/env python3
"""配布物の依存のライセンスと禁止部品を検査する（AC-25、REQ-007 AC-R7-03、ADR-0004）。標準ライブラリだけ。

    license_check.py SBOM.json [--exceptions tools/release/license-exceptions.txt] [--binary PATH ...] [--strict]

検査:
1. Rust の部品（purl が pkg:cargo）: Cargo.toml の SPDX 式を評価する。
   - 許可: MIT・Apache-2.0・BSD-2-Clause・BSD-3-Clause・ISC・Unicode-3.0・Zlib・CC0-1.0
   - 要確認（REVIEW）: MPL-2.0（ファイル単位の copyleft。改変しない・ソースの所在を示すなら可。法務の確認を記録する）
   - それ以外・不明・空: 不可（FAIL）
   OR は良い方、AND は悪い方。`WITH` の例外は LLVM-exception だけ許可（他は REVIEW）。
2. 禁止部品（Rust の crate 名・OS のパッケージ名）: ソフトウェアの H.264/H.265/AAC コーデックと FFmpeg の bindings
   （openh264・x264・x265・ffmpeg・fdk-aac・libav* など）。見つかれば FAIL（例外にできない）。
3. --binary: 実行ファイルの NEEDED と、dlopen しうるライブラリ名の文字列に禁止部品がないこと。
4. OS の部品のライセンスは Debian の copyright の欄（SPDX とは限らない）を一覧に出すだけ（別の著作物として配布。
   GPL の部品はソースの入手先を release の記録に残す: docs/RELEASE.md）。

例外ファイル（1 行 1 件、# はコメント）:
    <crate 名>[@<版>]  <ALLOW|REVIEWED>  <理由（誰がいつ確認したか）>
ALLOW は FAIL を通す（例: ライセンスの欄が空でファイルで確認したもの）。REVIEWED は REVIEW を確認済みにする。
終了コード: FAIL があれば 1。--strict なら未確認の REVIEW も 1。
"""

import argparse
import json
import re
import subprocess
import sys

ALLOWED = {"MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0", "Zlib", "CC0-1.0"}
REVIEW = {"MPL-2.0"}
ALLOWED_EXCEPTIONS = {"LLVM-exception"}

# 禁止部品（名前の正規表現。小文字で照合する）
FORBIDDEN = [
    (r"openh264", "OpenH264（H.264 のソフトウェア実装）"),
    (r"(^|[-_]|lib)x264($|[-_0-9])", "x264（H.264 の符号化器）"),
    (r"(^|[-_]|lib)x265($|[-_0-9])", "x265（H.265 の符号化器）"),
    (r"ffmpeg", "FFmpeg の bindings"),
    (r"(^|[-_])rsmpeg($|[-_])", "FFmpeg の bindings"),
    (r"^libav(codec|format|util|filter|device|resample)?([-_]?sys)?($|[0-9-])", "libav*（FFmpeg のライブラリ）"),
    (r"^libsw(scale|resample)", "FFmpeg のライブラリ"),
    (r"^libpostproc", "FFmpeg のライブラリ"),
    (r"fdk[-_]?aac", "fdk-aac（AAC の実装）"),
    (r"(^|[-_]|lib)(faad2?|faac)($|[-_0-9])", "FAAD/FAAC（AAC の実装）"),
    (r"(^|[-_]|lib)(de265|openhevc|kvazaar|svt[-_]?hevc)($|[-_0-9])", "H.265 のソフトウェア実装"),
]
# 実行ファイルに現れてはいけないライブラリ名（NEEDED・dlopen の文字列）
FORBIDDEN_SONAMES = [b"libavcodec.so", b"libavformat.so", b"libavutil.so", b"libx264.so", b"libx265.so",
                     b"libopenh264.so", b"libfdk-aac.so", b"libde265.so", b"libfaad.so"]

RANK = {"OK": 0, "REVIEW": 1, "FAIL": 2}


def tokenize(expr):
    return re.findall(r"\(|\)|[A-Za-z0-9.+\-]+", expr)


def evaluate(expr):
    """SPDX 式を評価して (状態, 理由) を返す。"""
    if not expr or not expr.strip():
        return "FAIL", "ライセンスの記載がない"
    # Cargo の古い書き方（MIT/Apache-2.0）
    if "/" in expr and " " not in expr.strip():
        expr = expr.replace("/", " OR ")
    toks = tokenize(expr)
    pos = 0

    def peek():
        return toks[pos] if pos < len(toks) else None

    def take():
        nonlocal pos
        t = toks[pos]
        pos += 1
        return t

    def atom():
        t = take()
        if t == "(":
            r = or_expr()
            if peek() == ")":
                take()
            return r
        lic = t.rstrip("+")
        st = "OK" if lic in ALLOWED else "REVIEW" if lic in REVIEW else "FAIL"
        why = lic
        if peek() == "WITH":
            take()
            exc = take()
            if exc not in ALLOWED_EXCEPTIONS and st == "OK":
                st = "REVIEW"
            why = f"{lic} WITH {exc}"
        return st, why

    def and_expr():
        st, why = atom()
        while peek() == "AND":
            take()
            s2, w2 = atom()
            if RANK[s2] > RANK[st]:
                st, why = s2, w2
        return st, why

    def or_expr():
        st, why = and_expr()
        while peek() == "OR":
            take()
            s2, w2 = and_expr()
            if RANK[s2] < RANK[st]:
                st, why = s2, w2
        return st, why

    try:
        st, why = or_expr()
        if pos != len(toks):
            return "FAIL", f"式を読めない: {expr}"
        return st, why
    except IndexError:
        return "FAIL", f"式を読めない: {expr}"


def forbidden(name):
    n = name.lower()
    for pat, why in FORBIDDEN:
        if re.search(pat, n):
            return why
    return None


def load_exceptions(path):
    ex = {}
    if not path:
        return ex
    try:
        with open(path) as f:
            for line in f:
                line = line.split("#", 1)[0].strip()
                if not line:
                    continue
                parts = line.split(None, 2)
                if len(parts) < 3 or parts[1] not in ("ALLOW", "REVIEWED"):
                    sys.exit(f"license_check: 例外の行が読めない: {line}")
                ex[parts[0]] = (parts[1], parts[2])
    except FileNotFoundError:
        pass
    return ex


def exception_for(ex, name, version):
    return ex.get(f"{name}@{version}") or ex.get(name)


def check_binary(path):
    problems = []
    try:
        out = subprocess.run(["readelf", "-d", path], capture_output=True, text=True, check=False).stdout
        for m in re.finditer(r"\(NEEDED\)\s+Shared library: \[(.+?)\]", out):
            if forbidden(m.group(1)) or any(m.group(1).encode().startswith(s) for s in FORBIDDEN_SONAMES):
                problems.append(f"NEEDED {m.group(1)}")
    except FileNotFoundError:
        problems.append("readelf がない（NEEDED を確かめられない）")
    with open(path, "rb") as f:
        data = f.read()
    for s in FORBIDDEN_SONAMES:
        if s in data:
            problems.append(f"文字列 {s.decode()}（dlopen しうる）")
    return problems


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("sbom")
    ap.add_argument("--exceptions")
    ap.add_argument("--binary", action="append", default=[])
    ap.add_argument("--strict", action="store_true")
    a = ap.parse_args()
    with open(a.sbom) as f:
        bom = json.load(f)
    ex = load_exceptions(a.exceptions)

    fails, reviews, oks = [], [], 0
    os_licenses = {}
    for c in bom.get("components", []):
        purl = c.get("purl", "")
        name, ver = c.get("name", ""), c.get("version", "")
        why = forbidden(name)
        if why and c.get("type") != "container":
            fails.append(f"禁止部品 {name}@{ver}: {why}")
            continue
        if purl.startswith("pkg:cargo/"):
            exprs = [l.get("expression") or (l.get("license") or {}).get("id") or "" for l in c.get("licenses", [])]
            expr = " AND ".join(f"({e})" for e in exprs if e) if len(exprs) > 1 else (exprs[0] if exprs else "")
            st, reason = evaluate(expr)
            e = exception_for(ex, name, ver)
            if st == "FAIL" and e and e[0] == "ALLOW":
                st, reason = "OK", f"例外: {e[1]}"
            if st == "REVIEW" and e and e[0] == "REVIEWED":
                st, reason = "OK", f"確認済み: {e[1]}"
            scope = c.get("scope", "")
            line = f"{name}@{ver} [{scope}] license={expr or '(なし)'}: {reason}"
            if st == "FAIL":
                fails.append(line)
            elif st == "REVIEW":
                reviews.append(line)
            else:
                oks += 1
        elif purl.startswith("pkg:deb/"):
            for l in c.get("licenses", []):
                n = (l.get("license") or {}).get("name")
                if n:
                    os_licenses.setdefault(n, set()).add(name)

    for b in a.binary:
        for p in check_binary(b):
            fails.append(f"{b}: {p}")

    print(f"license_check: Rust の部品 {oks + len(fails) + len(reviews)} 個を検査（OK {oks}、REVIEW {len(reviews)}、FAIL {len(fails)}）")
    if os_licenses:
        print(f"OS の部品のライセンス欄（参考、{len(os_licenses)} 種）:")
        for n in sorted(os_licenses):
            pk = sorted(os_licenses[n])
            print(f"  {n}: {', '.join(pk[:6])}{' ...' if len(pk) > 6 else ''}")
    for r in reviews:
        print(f"REVIEW {r}")
    for f_ in fails:
        print(f"FAIL   {f_}")
    if fails:
        print("FAIL: 許可外のライセンスまたは禁止部品がある")
        return 1
    if reviews and a.strict:
        print("FAIL: 未確認の REVIEW がある（--strict）")
        return 1
    print("PASS: 許可外のライセンス・禁止部品なし" + ("（REVIEW は要確認）" if reviews else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
