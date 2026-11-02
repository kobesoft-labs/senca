#!/usr/bin/env bash
# senca-http の相互運用試験。例のサーバー（crates/senca-http/examples/http_server.rs）に
# curl（keep-alive・chunked 送信・Expect: 100-continue・HTTP/1.0・HEAD）と、
# python3 の生ソケットで作った不正な要求（CL+TE の smuggling、巨大 header、壊れた chunk など）を送る。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
PORT="${PORT:-42080}"
BASE="http://127.0.0.1:$PORT"

WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
    rm -rf "$WORK"
}
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
check() { # check <名前> <期待> <実際>
    if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1 (expected '$2', got '$3')"; fi
}

"$CARGO" build -q --manifest-path "$ROOT/Cargo.toml" -p senca-http --example http_server
"$CARGO_TARGET_DIR/debug/examples/http_server" "$PORT" 2>"$WORK/server.log" &
SERVER_PID=$!
for _ in $(seq 1 50); do
    if curl -s -o /dev/null "$BASE/hello"; then break; fi
    sleep 0.1
done

# --- curl ---
check "GET /hello" "hello" "$(curl -s --http1.1 "$BASE/hello")"
check "GET path param" '{"id":"ab c"}' "$(curl -s "$BASE/streams/ab%20c")"
check "404" "404" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/nope")"
check "405" "405" "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/hello")"
check "bad path (%2F) -> 400" "400" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/streams/a%2Fb")"

curl -sv --http1.1 "$BASE/hello" "$BASE/streams/x" "$BASE/hello" >"$WORK/ka.out" 2>"$WORK/ka.err"
if grep -qiE "Re-?using existing" "$WORK/ka.err" && [[ "$(grep -o 'hello' "$WORK/ka.out" | wc -l)" == 2 ]]; then
    pass "keep-alive: 3 requests on one connection"
else
    fail "keep-alive"
fi

head -c 300000 /dev/urandom >"$WORK/small.bin"
curl -s -H 'Transfer-Encoding: chunked' --data-binary @"$WORK/small.bin" -o "$WORK/echo.bin" "$BASE/echo"
check "chunked upload echo" "$(sha256sum <"$WORK/small.bin")" "$(sha256sum <"$WORK/echo.bin")"

curl -s -X PUT --data-binary @"$WORK/small.bin" -o "$WORK/echo2.bin" "$BASE/echo"
check "content-length upload echo" "$(sha256sum <"$WORK/small.bin")" "$(sha256sum <"$WORK/echo2.bin")"

head -c 3000000 /dev/urandom >"$WORK/big.bin"
out="$(curl -sv -H 'Expect: 100-continue' --data-binary @"$WORK/big.bin" "$BASE/upload" 2>"$WORK/exp.err")"
if grep -q "HTTP/1.1 100 Continue" "$WORK/exp.err" && [[ "$out" == "3000000" ]]; then
    pass "Expect: 100-continue upload (3 MB)"
else
    fail "Expect: 100-continue upload (got '$out')"
fi
check "body over limit -> 413" "413" "$(curl -s -o /dev/null -w '%{http_code}' --data-binary @"$WORK/big.bin" "$BASE/echo")"
check "chunked body over limit -> 413" "413" \
    "$(curl -s -o /dev/null -w '%{http_code}' -H 'Transfer-Encoding: chunked' -H 'Expect:' --data-binary @"$WORK/big.bin" "$BASE/echo")"

check "HTTP/1.0" "hello" "$(curl -s -0 "$BASE/hello")"
check "HEAD Content-Length" "6" "$(curl -sI "$BASE/hello" | tr -d '\r' | awk -F': ' 'tolower($1)=="content-length"{print $2}')"
check "chunked response" "part0 part1 part2" "$(curl -s "$BASE/chunked" | tr '\n' ' ' | sed 's/ $//')"
check "chunked response to HTTP/1.0 (close-delimited)" "part0 part1 part2" "$(curl -s -0 "$BASE/chunked" | tr '\n' ' ' | sed 's/ $//')"
check "SSE" "3" "$(curl -sN "$BASE/events" | grep -c '^data: ')"

# --- 生ソケット ---
cat >"$WORK/raw.py" <<'PY'
import re, socket, sys

port = int(sys.argv[1])

def send(raw, timeout=3.0):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    try:
        s.sendall(raw)
        try:
            s.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        data = b""
        while True:
            try:
                d = s.recv(65536)
            except (ConnectionResetError, socket.timeout):
                break
            if not d:
                break
            data += d
        return data
    finally:
        s.close()

def statuses(data):
    return [m.decode() for m in re.findall(rb"HTTP/1\.[01] (\d{3}) ", data)]

H = b"Host: x\r\n"
cases = [
    ("CL+TE smuggling", b"POST /echo HTTP/1.1\r\n" + H + b"Content-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /hello HTTP/1.1\r\n" + H + b"\r\n", ["400"]),
    ("TE+CL smuggling", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n8\r\nSMUGGLED\r\n0\r\n\r\n", ["400"]),
    ("TE obfuscated (space before colon)", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding : chunked\r\n\r\n0\r\n\r\n", ["400"]),
    ("TE: xchunked", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: xchunked\r\n\r\n0\r\n\r\n", ["501"]),
    ("TE: chunked, identity", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked, identity\r\n\r\n0\r\n\r\n", ["400"]),
    ("duplicate Content-Length", b"POST /echo HTTP/1.1\r\n" + H + b"Content-Length: 1\r\nContent-Length: 1\r\n\r\na", ["400"]),
    ("conflicting Content-Length list", b"POST /echo HTTP/1.1\r\n" + H + b"Content-Length: 1, 2\r\n\r\na", ["400"]),
    ("oversize header -> 431", b"GET /hello HTTP/1.1\r\n" + H + b"X-Big: " + b"a" * 40000 + b"\r\n\r\n", ["431"]),
    ("too many headers -> 431", b"GET /hello HTTP/1.1\r\n" + H + b"".join(b"X-%d: v\r\n" % i for i in range(150)) + b"\r\n", ["431"]),
    ("long URI -> 414", b"GET /" + b"a" * 9000 + b" HTTP/1.1\r\n" + H + b"\r\n", ["414"]),
    ("bad chunk size -> 400", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked\r\n\r\nzz\r\nabc\r\n0\r\n\r\n", ["400"]),
    ("chunk size overflow -> 400", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked\r\n\r\n10000000000000000\r\n", ["400"]),
    ("missing CRLF after chunk -> 400", b"POST /echo HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked\r\n\r\n3\r\nabcX\r\n0\r\n\r\n", ["400"]),
    ("obs-fold -> 400", b"GET /hello HTTP/1.1\r\n" + H + b"X: a\r\n b\r\n\r\n", ["400"]),
    ("bare LF -> 400", b"GET /hello HTTP/1.1\n" + H.replace(b"\r\n", b"\n") + b"\n", ["400"]),
    ("bare CR -> 400", b"GET /hello HTTP/1.1\r\nX: a\rb\r\n" + H + b"\r\n", ["400"]),
    ("NUL in header -> 400", b"GET /hello HTTP/1.1\r\n" + H + b"X: a\x00b\r\n\r\n", ["400"]),
    ("missing Host -> 400", b"GET /hello HTTP/1.1\r\n\r\n", ["400"]),
    ("HTTP/2.0 -> 505", b"GET /hello HTTP/2.0\r\n" + H + b"\r\n", ["505"]),
    ("Expect: other -> 417", b"POST /echo HTTP/1.1\r\n" + H + b"Expect: 200-ok\r\nContent-Length: 1\r\n\r\na", ["417"]),
    ("pipelined 3 requests", (b"GET /hello HTTP/1.1\r\n" + H + b"\r\n") * 2 + b"POST /echo HTTP/1.1\r\n" + H + b"Content-Length: 2\r\nConnection: close\r\n\r\nhi", ["200", "200", "200"]),
    ("request after Connection: close ignored", b"GET /hello HTTP/1.1\r\n" + H + b"Connection: close\r\n\r\nGET /hello HTTP/1.1\r\n" + H + b"\r\n", ["200"]),
]
ok = True
for name, raw, want in cases:
    got = statuses(send(raw))
    if got == want:
        print("PASS: raw: " + name)
    else:
        print("FAIL: raw: %s (expected %s, got %s)" % (name, want, got))
        ok = False
sys.exit(0 if ok else 1)
PY
if python3 -I "$WORK/raw.py" "$PORT" >"$WORK/raw.out"; then :; fi
cat "$WORK/raw.out"
PASS=$((PASS + $(grep -c '^PASS' "$WORK/raw.out" || true)))
FAIL=$((FAIL + $(grep -c '^FAIL' "$WORK/raw.out" || true)))

if ! kill -0 "$SERVER_PID" 2>/dev/null; then fail "server died"; fi

echo "----"
echo "passed: $PASS, failed: $FAIL"
[[ "$FAIL" == 0 ]]
