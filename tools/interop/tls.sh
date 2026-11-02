#!/usr/bin/env bash
# senca-tls（TLS 1.3）と OpenSSL 3.5・curl・FFmpeg(GnuTLS)・Python ssl の相互接続試験。
#
#   A. 自前の client（examples/tls_client）→ openssl s_server -tls1_3 -www
#      スイート×group、鍵の種類（ECDSA・RSA・Ed25519）、HRR、チケット再開、mTLS、ALPN、KeyUpdate、
#      証明書の不正（信頼しない CA・ホスト名違い）、TLS 1.2 だけの server（失敗が正しい）。
#   B. openssl s_client -tls1_3 → 自前の server（examples/tls_server）
#      スイート×group、鍵の種類、SNI での証明書選択、HRR、-sess_out/-sess_in の再開、mTLS、ALPN、
#      KeyUpdate（両方向）、TLS 1.2 だけの client（失敗が正しい）。
#   C. curl --tlsv1.3 --cacert → 自前の server（スイート・curve・SNI・ALPN）。
#   D. 参考: FFmpeg（GnuTLS）の https/tls、Python ssl → 自前の server。
#
# 証明書はこのスクリプトが openssl で作る。ポートは 48000-48999 を使う。
# 環境変数: CARGO、CARGO_TARGET_DIR、FFMPEG（既定 ~/tools/ffmpeg）、TLS_PORT_BASE（既定 48100）。
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
PORT=${TLS_PORT_BASE:-48100}
export OPENSSL_CONF=/dev/null

WORK=$(mktemp -d)
PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
# expect <名前> <ファイル> <正規表現>...（すべて含まれれば PASS）
expect() {
    local name=$1 file=$2; shift 2
    for re in "$@"; do
        if ! grep -qE -- "$re" "$file"; then
            fail "$name (missing '$re')"
            sed 's/^/    | /' "$file" | tail -n 8
            return
        fi
    done
    pass "$name"
}
# reject <名前> <終了コード> <ファイル> <正規表現>（失敗して、出力に re があれば PASS）
reject() {
    local name=$1 rc=$2 file=$3 re=$4
    if [[ "$rc" != 0 ]] && grep -qE -- "$re" "$file"; then
        pass "$name"
    else
        fail "$name (rc=$rc, expected failure with '$re')"
        sed 's/^/    | /' "$file" | tail -n 8
    fi
}
next_port() { PORT=$((PORT + 1)); if ((PORT > 48999)); then echo "ポートが尽きた" >&2; exit 1; fi; }
wait_port() {
    for _ in $(seq 1 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then return 0; fi
        sleep 0.05
    done
    echo "port $1 が開かない" >&2
    return 1
}
start() { # start <ログ> <コマンド...>（PORT で待ち受けるもの）
    local log=$1; shift
    "$@" >"$log" 2>&1 &
    PIDS+=($!)
    wait_port "$PORT"
}
stop_last() {
    local p=${PIDS[-1]}
    kill "$p" 2>/dev/null || true
    wait "$p" 2>/dev/null || true
    unset 'PIDS[-1]'
}

# ---------------------------------------------------------------------------
# 証明書
# ---------------------------------------------------------------------------
cd "$WORK"
cat >ext.cnf <<'CNF'
[req]
distinguished_name=dn
[dn]
[v3_root]
basicConstraints=critical,CA:TRUE
keyUsage=critical,keyCertSign,cRLSign
subjectKeyIdentifier=hash
[v3_inter]
basicConstraints=critical,CA:TRUE,pathlen:0
keyUsage=critical,keyCertSign,cRLSign
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
[v3_server]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,DNS:example.test,IP:127.0.0.1
authorityKeyIdentifier=keyid
[v3_client]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=DNS:node1.senca.internal
authorityKeyIdentifier=keyid
CNF
q() { "$@" >/dev/null 2>&1; }
ec() { q openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1"; }
ec ca.key
q openssl req -config ext.cnf -x509 -new -key ca.key -subj "/CN=Interop Root" -days 30 -sha256 -extensions v3_root -out ca.pem
ec inter.key
q openssl req -config ext.cnf -new -key inter.key -subj "/CN=Interop Intermediate" -out t.csr
q openssl x509 -req -in t.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -sha256 -extfile ext.cnf -extensions v3_inter -out inter.pem
ec ecdsa.key
q openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out rsa.key
q openssl genpkey -algorithm ED25519 -out ed25519.key
for k in ecdsa rsa ed25519; do
    q openssl req -config ext.cnf -new -key "$k.key" -subj "/CN=localhost" -out t.csr
    q openssl x509 -req -in t.csr -CA inter.pem -CAkey inter.key -CAcreateserial -days 30 -sha256 -extfile ext.cnf -extensions v3_server -out "$k.crt"
    cat "$k.crt" inter.pem >"$k.pem"
done
ec client.key
q openssl req -config ext.cnf -new -key client.key -subj "/CN=node1" -out t.csr
q openssl x509 -req -in t.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -sha256 -extfile ext.cnf -extensions v3_client -out client.pem
ec other.key
q openssl req -config ext.cnf -x509 -new -key other.key -subj "/CN=Other Root" -days 30 -sha256 -extensions v3_root -out other-ca.pem

"$CARGO" build -q --manifest-path "$ROOT/Cargo.toml" -p senca-tls --examples
CLIENT="$CARGO_TARGET_DIR/debug/examples/tls_client"
SERVER="$CARGO_TARGET_DIR/debug/examples/tls_server"

suite_name() {
    case $1 in
        aes128) echo TLS_AES_128_GCM_SHA256 ;;
        aes256) echo TLS_AES_256_GCM_SHA384 ;;
        chacha) echo TLS_CHACHA20_POLY1305_SHA256 ;;
    esac
}

# ---------------------------------------------------------------------------
# A. 自前の client → openssl s_server
# ---------------------------------------------------------------------------
s_server() { # s_server <ログ> <追加引数...>
    local log=$1; shift
    next_port
    start "$log" openssl s_server -tls1_3 -accept "$PORT" -www -cert_chain inter.pem "$@"
}
client() { # client <出力> <追加引数...>（終了コードを返す）
    local out=$1; shift
    local rc=0
    timeout 20 "$CLIENT" --connect "127.0.0.1:$PORT" --ca ca.pem "$@" >"$out" 2>&1 || rc=$?
    return $rc
}

for s in aes128 aes256 chacha; do
    for g in X25519 P-256; do
        name=$(suite_name $s)
        s_server "$WORK/a.log" -cert ecdsa.crt -key ecdsa.key -ciphersuites "$name" -groups "$g"
        client "$WORK/a.out" --name localhost || true
        want_g=$([[ $g == X25519 ]] && echo X25519 || echo Secp256r1)
        expect "A client->s_server $s $g" "$WORK/a.out" "handshake ok: suite=$name group=Some\($want_g\)" "Cipher is $name" "close_notify received: true"
        stop_last
    done
done
for k in rsa ed25519; do
    s_server "$WORK/a.log" -cert "$k.crt" -key "$k.key"
    client "$WORK/a.out" --name localhost || true
    expect "A client->s_server cert=$k" "$WORK/a.out" "handshake ok" "New, TLSv1.3"
    stop_last
done

s_server "$WORK/a.log" -cert ecdsa.crt -key ecdsa.key -groups P-256
client "$WORK/a.out" --name localhost --groups x25519,p256 || true
expect "A client->s_server HRR (server P-256 only)" "$WORK/a.out" "group=Some\(Secp256r1\) resumed=false hrr=true"
stop_last

s_server "$WORK/a.log" -cert ecdsa.crt -key ecdsa.key
client "$WORK/a.out" --name localhost --count 3 || true
expect "A client->s_server ticket resumption" "$WORK/a.out" "resumed=false" "resumed=true" "Reused, TLSv1.3"
client "$WORK/a.out" --name localhost --key-update || true
expect "A client->s_server KeyUpdate(update_requested)" "$WORK/a.out" "handshake ok" "Cipher is" "close_notify received: true"
client "$WORK/a.out" --name localhost --record-size-limit 512 || true
expect "A client->s_server record_size_limit offered" "$WORK/a.out" "handshake ok" "Cipher is"
rc=0; client "$WORK/a.out" --name wrong.test || rc=$?
reject "A client rejects wrong hostname" $rc "$WORK/a.out" "CertNotValidForName"
rc=0; timeout 20 "$CLIENT" --connect "127.0.0.1:$PORT" --ca other-ca.pem --name localhost >"$WORK/a.out" 2>&1 || rc=$?
reject "A client rejects untrusted CA" $rc "$WORK/a.out" "UnknownIssuer"
pin=$(openssl x509 -in ecdsa.crt -pubkey -noout | openssl pkey -pubin -outform DER | openssl dgst -sha256 -hex | awk '{print $NF}')
rc=0; timeout 20 "$CLIENT" --connect "127.0.0.1:$PORT" --pin "$pin" --name anything.internal >"$WORK/a.out" 2>&1 || rc=$?
expect "A client SPKI pin only" "$WORK/a.out" "handshake ok"
rc=0; client "$WORK/a.out" --name localhost --pin 00000000000000000000000000000000000000000000000000000000000000ff || rc=$?
reject "A client rejects wrong SPKI pin" $rc "$WORK/a.out" "pin"
stop_last

s_server "$WORK/a.log" -cert ecdsa.crt -key ecdsa.key -Verify 1 -CAfile ca.pem
client "$WORK/a.out" --name localhost --cert client.pem --key client.key || true
expect "A client->s_server mTLS" "$WORK/a.out" "handshake ok" "Cipher is"
rc=0; client "$WORK/a.out" --name localhost || rc=$?
reject "A client without cert rejected by s_server -Verify" $rc "$WORK/a.out" "error"
stop_last

s_server "$WORK/a.log" -cert ecdsa.crt -key ecdsa.key -alpn h3,http/1.1
client "$WORK/a.out" --name localhost --alpn http/1.1 || true
expect "A client->s_server ALPN" "$WORK/a.out" "alpn=http/1.1"
stop_last

next_port
start "$WORK/a.log" openssl s_server -tls1_2 -accept "$PORT" -www -cert ecdsa.crt -key ecdsa.key -cert_chain inter.pem
rc=0; client "$WORK/a.out" --name localhost || rc=$?
reject "A client refuses TLS 1.2 server" $rc "$WORK/a.out" "protocol_version|illegal_parameter"
stop_last

# ---------------------------------------------------------------------------
# B. openssl s_client → 自前の server
# ---------------------------------------------------------------------------
server() { # server <ログ> <追加引数...>
    local log=$1; shift
    next_port
    start "$log" "$SERVER" --port "$PORT" "$@"
}
s_client() { # s_client <出力> <stdin 文字列> <追加引数...>
    local out=$1 input=$2; shift 2
    printf '%b' "$input" | timeout 10 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -CAfile ca.pem -verify_return_error -ign_eof "$@" >"$out" 2>&1 || true
}
GET='GET / HTTP/1.0\r\n\r\n'

server "$WORK/b.log" --cert ecdsa.pem --key ecdsa.key --sni example.test=ed25519.pem:ed25519.key --alpn h3,http/1.1,moq-00
for s in aes128 aes256 chacha; do
    for g in X25519 P-256; do
        name=$(suite_name $s)
        s_client "$WORK/b.out" "$GET" -servername localhost -ciphersuites "$name" -groups "$g"
        want_g=$([[ $g == X25519 ]] && echo X25519 || echo Secp256r1)
        expect "B s_client->server $s $g" "$WORK/b.out" "Cipher is $name" "Verify return code: 0" "suite=$name group=Some\($want_g\)"
    done
done
s_client "$WORK/b.out" "$GET" -servername example.test
expect "B SNI selects Ed25519 cert" "$WORK/b.out" "Peer signature type: [Ee][Dd]25519" "sni=example.test"
s_client "$WORK/b.out" "$GET" -servername localhost -alpn h3
expect "B ALPN h3" "$WORK/b.out" "ALPN protocol: h3" "alpn=h3"
s_client "$WORK/b.out" "$GET" -servername localhost -alpn moq-00
expect "B ALPN moq-00" "$WORK/b.out" "ALPN protocol: moq-00"
s_client "$WORK/b.out" "$GET" -servername localhost -alpn spdy/3
expect "B ALPN mismatch -> no_application_protocol" "$WORK/b.out" "alert no application protocol|no application protocol"
s_client "$WORK/b.out" "$GET" -servername localhost -sess_out "$WORK/sess.pem"
s_client "$WORK/b.out" "$GET" -servername localhost -sess_in "$WORK/sess.pem"
expect "B resumption -sess_out/-sess_in" "$WORK/b.out" "Reused, TLSv1.3" "resumed=true"
# 接続中のコマンド（K = KeyUpdate(update_requested)）は -ign_eof では効かないので、stdin を開いたままにする。
(printf 'K\n'; sleep 0.5; printf "$GET"; sleep 1) | timeout 10 openssl s_client -tls1_3 -msg -connect "127.0.0.1:$PORT" -CAfile ca.pem -servername localhost >"$WORK/b.out" 2>&1 || true
expect "B KeyUpdate from s_client (update_requested) answered" "$WORK/b.out" ">>> TLS 1.3, Handshake \[length 0005\], KeyUpdate" "<<< TLS 1.3, Handshake \[length 0005\], KeyUpdate" "HTTP/1.1 200 OK"
printf "$GET" | timeout 10 openssl s_client -tls1_2 -connect "127.0.0.1:$PORT" -CAfile ca.pem -servername localhost >"$WORK/b.out" 2>&1 || true
expect "B TLS 1.2-only s_client is refused" "$WORK/b.out" "alert protocol version|protocol version"
stop_last

server "$WORK/b.log" --cert rsa.pem --key rsa.key --groups p256 --key-update
s_client "$WORK/b.out" "$GET" -servername localhost -groups X25519:P-256 -msg
expect "B HRR (s_client X25519 share, server P-256 only)" "$WORK/b.out" "hrr=true" "Peer signature type: rsa_pss_rsae_sha256"
# 応答は新しい鍵で暗号化している。OpenSSL は自分の KeyUpdate を次の送信まで遅らせるので、受信だけを見る。
expect "B KeyUpdate from server (update_requested)" "$WORK/b.out" "<<< TLS 1.3, Handshake \[length 0005\], KeyUpdate" "HTTP/1.1 200 OK"
s_client "$WORK/b.out" "$GET" -servername localhost -groups P-256
expect "B P-256 share accepted directly" "$WORK/b.out" "hrr=false" "group=Some\(Secp256r1\)"
stop_last

server "$WORK/b.log" --cert ecdsa.pem --key ecdsa.key --client-ca ca.pem
s_client "$WORK/b.out" "$GET" -servername localhost -cert client.pem -key client.key
expect "B mTLS (client cert verified)" "$WORK/b.out" "client_cert=yes"
s_client "$WORK/b.out" "$GET" -servername localhost
expect "B mTLS without cert -> certificate_required" "$WORK/b.out" "certificate required"
stop_last

# ---------------------------------------------------------------------------
# C. curl → 自前の server
# ---------------------------------------------------------------------------
server "$WORK/c.log" --cert ecdsa.pem --key ecdsa.key --sni example.test=rsa.pem:rsa.key --alpn http/1.1
for s in aes128 aes256 chacha; do
    name=$(suite_name $s)
    curl -s --tlsv1.3 --tls13-ciphers "$name" --cacert ca.pem "https://localhost:$PORT/" >"$WORK/c.out" 2>&1 || true
    expect "C curl $s" "$WORK/c.out" "suite=$name"
done
curl -s --tlsv1.3 --curves P-256 --cacert ca.pem "https://localhost:$PORT/" >"$WORK/c.out" 2>&1 || true
expect "C curl --curves P-256" "$WORK/c.out" "group=Some\(Secp256r1\)"
curl -s --tlsv1.3 --cacert ca.pem --resolve "example.test:$PORT:127.0.0.1" "https://example.test:$PORT/" >"$WORK/c.out" 2>&1 || true
expect "C curl SNI example.test + ALPN" "$WORK/c.out" "sni=example.test" "alpn=http/1.1"
curl -s --tlsv1.3 --cacert ca.pem "https://127.0.0.1:$PORT/" >"$WORK/c.out" 2>&1 || true
expect "C curl to IP address (no SNI)" "$WORK/c.out" "sni=-"
stop_last

# ---------------------------------------------------------------------------
# D. 参考: FFmpeg（GnuTLS）と Python ssl
# ---------------------------------------------------------------------------
server "$WORK/d.log" --cert ecdsa.pem --key ecdsa.key
if [[ -x "$FFMPEG" ]]; then
    # 静的ビルドの FFmpeg 7.0.2 は名前解決（localhost）・-ca_file・rtmps:// で（相手が OpenSSL でも）
    # 落ちるので、IP アドレスへ検証なしの https でハンドシェイクだけを確かめる。
    timeout 10 "$FFMPEG" -hide_banner -tls_verify 0 -i "https://127.0.0.1:$PORT/" -f null - >"$WORK/d.out" 2>&1 || true
    expect "D ffmpeg(GnuTLS) https handshake" "$WORK/d.log" "handshake ok"
fi
python3 - "$PORT" >"$WORK/d.out" 2>&1 <<'PY' || true
import socket, ssl, sys
ctx = ssl.create_default_context(cafile="ca.pem")
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
with socket.create_connection(("127.0.0.1", int(sys.argv[1]))) as s:
    with ctx.wrap_socket(s, server_hostname="localhost") as t:
        t.sendall(b"GET / HTTP/1.0\r\n\r\n")
        data = b""
        while True:
            d = t.recv(4096)
            if not d:
                break
            data += d
        print(t.version(), t.cipher()[0])
        print(data.decode())
PY
expect "D python ssl" "$WORK/d.out" "TLSv1.3" "HTTP/1.1 200 OK"
stop_last

echo "== $PASS passed, $FAIL failed"
[[ $FAIL == 0 ]]
