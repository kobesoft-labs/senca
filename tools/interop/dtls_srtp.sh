#!/usr/bin/env bash
# DTLS 1.2（senca-dtls）と SRTP（senca-srtp）の相互接続試験。
# 相手: OpenSSL 3.x の s_server / s_client（DTLS-SRTP・exporter）、FFmpeg の srtp（AES_CM_128_HMAC_SHA1_80/32）。
# 試験の相手としてだけ使い、製品には入れない。
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
OPENSSL=${OPENSSL:-openssl}
CARGO=${CARGO:-$HOME/.cargo/bin/cargo}

W=$(mktemp -d)
cleanup() {
  local p
  for p in $(jobs -p); do kill "$p" 2>/dev/null || true; done
  if [ -n "${KEEP:-}" ]; then echo "作業ディレクトリ: $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT

(cd "$ROOT" && "$CARGO" build -q -p senca-dtls -p senca-srtp --examples)
EX=$TARGET/debug/examples
PEER=$EX/dtls_peer
TOOL=$EX/srtp_tool

# 47000〜47999 の範囲で使う
NEXT_PORT=$((47000 + (RANDOM % 80) * 10))
# サブシェルで呼ぶと数が進まないので、結果は変数 P に置く
next_port() { NEXT_PORT=$((NEXT_PORT + 1)); P=$NEXT_PORT; }
wait_udp() { local i; for i in $(seq 200); do ss -uln 2>/dev/null | grep -q ":$1 " && return 0; sleep 0.02; done; return 1; }

cd "$W"
"$OPENSSL" req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout k.pem -out c.pem \
  -days 2 -subj /CN=openssl-peer >/dev/null 2>&1
OSSL_FP=$("$OPENSSL" x509 -in c.pem -noout -fingerprint -sha256 | cut -d= -f2)

ours_km() { awk '/^KEYING_MATERIAL/ {print $2}' "$1"; }
ossl_km() { awk '/Keying material:/ {print $3}' "$1"; }

check_dtls() { # name ours.log ossl.log expected_cipher
  local name=$1 o=$2 s=$3 cipher=$4 a b
  a=$(ours_km "$o"); b=$(ossl_km "$s")
  if [ -n "$a" ] && [ "$a" = "$b" ]; then pass "$name: SRTP keying material 一致 (${#a} hex)"; else fail "$name: keying material ours=$a openssl=$b"; fi
  if grep -q "^RECV hello-from-openssl" "$o"; then pass "$name: openssl→senca application data"; else fail "$name: openssl→senca"; fi
  if grep -q "hello-from-senca" "$s"; then pass "$name: senca→openssl application data"; else fail "$name: senca→openssl"; fi
  if grep -q "^PEER_FINGERPRINT sha-256 $OSSL_FP\$" "$o"; then pass "$name: 相手証明書の fingerprint"; else fail "$name: fingerprint"; fi
  if grep -q "$cipher" "$s"; then pass "$name: cipher $cipher"; else fail "$name: cipher $cipher"; fi
}

# ---- senca client → openssl s_server ----
client_case() { # name suite group our_srtp ossl_srtp kmlen cipher [extra s_server args]
  local name=$1 suite=$2 group=$3 srtp=$4 osrtp=$5 kmlen=$6 cipher=$7; shift 7
  local p; next_port; p=$P
  (sleep 2; echo hello-from-openssl; sleep 4) | timeout 12 "$OPENSSL" s_server -dtls1_2 -cert c.pem -key k.pem \
    -use_srtp "$osrtp" -port "$p" -keymatexport EXTRACTOR-dtls_srtp -keymatexportlen "$kmlen" -naccept 1 -verify 1 "$@" \
    >"$name.ossl" 2>&1 &
  local spid=$!
  wait_udp "$p" || true
  timeout 12 "$PEER" client "127.0.0.1:$p" --suite "$suite" --group "$group" --srtp "$srtp" \
    --send hello-from-senca --expect hello-from-openssl >"$name.ours" 2>&1 || true
  wait "$spid" 2>/dev/null || true
  check_dtls "$name" "$name.ours" "$name.ossl" "$cipher"
}

client_case c-aes128-x25519-gcm aes128 x25519 aes128gcm,cm80 SRTP_AEAD_AES_128_GCM 56 ECDHE-ECDSA-AES128-GCM-SHA256
client_case c-aes256-p256-cm80 aes256 p256 cm80 SRTP_AES128_CM_SHA1_80 60 ECDHE-ECDSA-AES256-GCM-SHA384
client_case c-chacha-x25519-gcm256 chacha x25519 aes256gcm SRTP_AEAD_AES_256_GCM 88 ECDHE-ECDSA-CHACHA20-POLY1305
# OpenSSL 側の cookie 交換（DTLSv1_listen）
client_case c-openssl-cookie aes128 p256 cm80,cm32 SRTP_AES128_CM_SHA1_32:SRTP_AES128_CM_SHA1_80 60 ECDHE-ECDSA-AES128-GCM-SHA256 -listen
grep -q "srtp=0x0002" c-openssl-cookie.ours && pass "c-openssl-cookie: server の優先順で SRTP_AES128_CM_SHA1_32" \
  || fail "c-openssl-cookie: SRTP profile"

# ---- openssl s_client → senca server ----
server_case() { # name suite our_srtp ossl_srtp kmlen cipher our_extra... -- ossl_extra...
  local name=$1 suite=$2 srtp=$3 osrtp=$4 kmlen=$5 cipher=$6; shift 6
  local ours=() ossl=()
  while [ $# -gt 0 ] && [ "$1" != "--" ]; do ours+=("$1"); shift; done
  [ $# -gt 0 ] && shift
  ossl=("$@")
  local p; next_port; p=$P
  timeout 14 "$PEER" server "127.0.0.1:$p" --suite "$suite" --srtp "$srtp" --send hello-from-senca \
    --expect hello-from-openssl "${ours[@]}" >"$name.ours" 2>&1 &
  local spid=$!
  wait_udp "$p" || true
  (sleep 2; echo hello-from-openssl; sleep 3) | timeout 12 "$OPENSSL" s_client -dtls1_2 -connect "127.0.0.1:$p" \
    -cert c.pem -key k.pem -use_srtp "$osrtp" -keymatexport EXTRACTOR-dtls_srtp -keymatexportlen "$kmlen" "${ossl[@]}" \
    >"$name.ossl" 2>&1 || true
  wait "$spid" 2>/dev/null || true
  check_dtls "$name" "$name.ours" "$name.ossl" "$cipher"
}

server_case s-aes128-cookie aes128 aes128gcm,cm80 SRTP_AES128_CM_SHA1_80:SRTP_AEAD_AES_128_GCM 56 ECDHE-ECDSA-AES128-GCM-SHA256 --cookie
grep -q HELLO_VERIFY_SENT s-aes128-cookie.ours && pass "s-aes128-cookie: HelloVerifyRequest を返した" || fail "s-aes128-cookie: cookie"
server_case s-aes256-p256 aes256 cm80 SRTP_AES128_CM_SHA1_80 60 ECDHE-ECDSA-AES256-GCM-SHA384 -- -groups P-256
grep -q "group=23" s-aes256-p256.ours && pass "s-aes256-p256: P-256" || fail "s-aes256-p256: group"
server_case s-chacha chacha aes256gcm SRTP_AEAD_AES_256_GCM 88 ECDHE-ECDSA-CHACHA20-POLY1305
# 小さい MTU（両方向で handshake を分割）
server_case s-mtu300 aes128 aes128gcm SRTP_AEAD_AES_128_GCM 56 ECDHE-ECDSA-AES128-GCM-SHA256 --cookie --mtu 300 -- -mtu 300

# ---- 拒否すべき相手 ----
reject_server_case() { # name reason_regex s_client args...
  local name=$1 re=$2; shift 2
  local p; next_port; p=$P
  timeout 8 "$PEER" server "127.0.0.1:$p" --timeout 6 >"$name.ours" 2>&1 &
  local spid=$!
  wait_udp "$p" || true
  (sleep 2) | timeout 6 "$OPENSSL" s_client -dtls1_2 -connect "127.0.0.1:$p" "$@" >"$name.ossl" 2>&1 || true
  wait "$spid" 2>/dev/null || true
  if grep -q -E "$re" "$name.ours" && ! grep -q '^CONNECTED' "$name.ours"; then pass "$name: 拒否"; else fail "$name: 拒否されない"; fi
}
reject_server_case r-client-no-ems "extended master secret" -cert c.pem -key k.pem -use_srtp SRTP_AEAD_AES_128_GCM -no_ems
reject_server_case r-client-cbc-only "共通の cipher suite" -cert c.pem -key k.pem -cipher ECDHE-ECDSA-AES128-SHA
# WebRTC では client 証明書が必須（証明書を持たない s_client は空の Certificate を返す）
reject_server_case r-client-no-cert "証明書が無い" -use_srtp SRTP_AEAD_AES_128_GCM
# P-256 を含まない supported_groups（ECDSA P-256 証明書を使えない。RFC 8422 5.1）
reject_server_case r-client-no-p256 "secp256r1" -cert c.pem -key k.pem -groups X25519

next_port; p=$P
(sleep 3) | timeout 6 "$OPENSSL" s_server -dtls1_2 -cert c.pem -key k.pem -port "$p" -no_ems -naccept 1 >r-server-no-ems.ossl 2>&1 &
spid=$!
wait_udp "$p" || true
timeout 6 "$PEER" client "127.0.0.1:$p" --timeout 4 >r-server-no-ems.ours 2>&1 || true
wait "$spid" 2>/dev/null || true
if grep -q "extended master secret" r-server-no-ems.ours && ! grep -q '^CONNECTED' r-server-no-ems.ours; then
  pass "r-server-no-ems: 拒否"; else fail "r-server-no-ems: 拒否されない"; fi

# 再ネゴシエーション（s_client の R）を受けない
next_port; p=$P
timeout 10 "$PEER" server "127.0.0.1:$p" --timeout 7 --expect never >r-reneg.ours 2>&1 &
spid=$!
wait_udp "$p" || true
(sleep 1.5; echo R; sleep 3) | timeout 8 "$OPENSSL" s_client -dtls1_2 -connect "127.0.0.1:$p" -cert c.pem -key k.pem \
  -use_srtp SRTP_AEAD_AES_128_GCM >r-reneg.ossl 2>&1 || true
wait "$spid" 2>/dev/null || true
if grep -q RENEGOTIATING r-reneg.ossl && [ "$(grep -c '^CONNECTED' r-reneg.ours)" = 1 ] && ! grep -q "handshake:" r-reneg.ours; then
  pass "r-reneg: 再ネゴシエーションを受けない"; else fail "r-reneg"; fi

# ---- SRTP: FFmpeg → senca ----
# 注: FFmpeg 7.0 の AES_CM_128_HMAC_SHA1_32 は SRTCP も 32 bit tag にする。RFC 5764 4.1.2（と libsrtp・
# ブラウザー）は SRTCP を 80 bit にするので、_32 の SRTCP は突き合わせない（RTP だけ両方向で確かめる）。
for suite in AES_CM_128_HMAC_SHA1_80 AES_CM_128_HMAC_SHA1_32; do
  key=$(head -c 30 /dev/urandom | base64)
  next_port; ps=$P; next_port; ps2=$P; next_port; pp=$P; next_port
  # srtp の RTCP は ps+1 に来る。平文の RTP は別ポート。
  "$TOOL" capture "$ps" 40 10000 "$suite.srtp" 2>/dev/null &
  c1=$!
  "$TOOL" capture "$ps2" 2 10000 "$suite.srtcp" 2>/dev/null &
  c2=$!
  "$TOOL" capture "$pp" 40 10000 "$suite.plain" 2>/dev/null &
  c3=$!
  wait_udp "$ps" && wait_udp "$ps2" && wait_udp "$pp" || true
  "$FFMPEG" -v error -re -f lavfi -i "sine=frequency=440:sample_rate=8000:duration=5" \
    -c:a pcm_mulaw -f rtp -srtp_out_suite "$suite" -srtp_out_params "$key" "srtp://127.0.0.1:$ps" \
    -c:a pcm_mulaw -f rtp "rtp://127.0.0.1:$pp" >/dev/null 2>&1 || true
  sleep 0.5
  kill "$c1" "$c2" "$c3" 2>/dev/null || true
  wait "$c1" "$c2" "$c3" 2>/dev/null || true
  "$TOOL" unprotect rtp "$suite" "$key" "$suite.srtp" "$suite.dec" >"$suite.unp" 2>/dev/null || true
  n=$(wc -l <"$suite.srtp")
  if [ "$n" -gt 10 ] && [ "$(cut -c25- "$suite.dec" | md5sum)" = "$(cut -c25- "$suite.plain" | md5sum)" ] \
    && grep -q "failed=0" "$suite.unp"; then
    pass "ffmpeg→senca $suite: $n packet を復号、payload が平文 RTP と一致"
  else
    fail "ffmpeg→senca $suite: $(cat "$suite.unp")"
  fi
  "$TOOL" unprotect rtcp "$suite" "$key" "$suite.srtcp" "$suite.rtcp" >"$suite.unpc" 2>/dev/null || true
  if [ "$suite" = AES_CM_128_HMAC_SHA1_32 ]; then
    echo "NOTE ffmpeg→senca $suite: FFmpeg の SRTCP tag は 32 bit（RFC 5764 は 80 bit）。senca は RFC に従い拒否: $(cat "$suite.unpc")"
  elif grep -q "ok=[1-9]" "$suite.unpc" && grep -q "failed=0" "$suite.unpc" && grep -q '^80c8' "$suite.rtcp"; then
    pass "ffmpeg→senca $suite: SRTCP（SR）を復号"
  else
    fail "ffmpeg→senca $suite: SRTCP $(cat "$suite.unpc")"
  fi
  # 改変（1 bit）と replay（同じ packet をもう一度）は拒否する
  python3 - "$suite.srtp" "$suite.bad" <<'EOF'
import sys
lines = [l.strip() for l in open(sys.argv[1]) if l.strip()]
b = bytearray.fromhex(lines[3]); b[20] ^= 0x01; lines[3] = b.hex()
lines.insert(6, lines[5])
open(sys.argv[2], "w").write("\n".join(lines) + "\n")
EOF
  "$TOOL" unprotect rtp "$suite" "$key" "$suite.bad" "$suite.baddec" >"$suite.badunp" 2>/dev/null || true
  if grep -q "failed=2" "$suite.badunp"; then pass "senca $suite: 改変と replay を拒否"; else fail "senca $suite: $(cat "$suite.badunp")"; fi

  # ---- SRTP: senca → FFmpeg ----
  key2=$(head -c 30 /dev/urandom | base64)
  "$TOOL" protect rtp "$suite" "$key2" "$suite.plain" "$suite.enc" >/dev/null
  python3 - "$suite.enc" "$suite.enctamper" <<'EOF'
import sys
lines = [l.strip() for l in open(sys.argv[1]) if l.strip()]
b = bytearray.fromhex(lines[4]); b[30] ^= 0x80; lines[4] = b.hex()
open(sys.argv[2], "w").write("\n".join(lines) + "\n")
EOF
  for variant in enc enctamper; do
    next_port; pr=$P; next_port
    cat >"$suite.$variant.sdp" <<EOF
v=0
o=- 0 0 IN IP4 127.0.0.1
s=senca
c=IN IP4 127.0.0.1
t=0 0
m=audio $pr RTP/SAVP 0
a=rtpmap:0 PCMU/8000
a=crypto:1 $suite inline:$key2
EOF
    rm -f "$suite.$variant.raw"
    timeout -s INT 8 "$FFMPEG" -v error -protocol_whitelist file,udp,rtp,srtp -i "$suite.$variant.sdp" \
      -c copy -f mulaw -y "$suite.$variant.raw" >/dev/null 2>&1 &
    fpid=$!
    wait_udp "$pr" || true
    sleep 0.3
    "$TOOL" send "$suite.$variant" "127.0.0.1:$pr" 20
    sleep 0.5
    kill -INT "$fpid" 2>/dev/null || true
    wait "$fpid" 2>/dev/null || true
  done
  res=$(python3 - "$suite.plain" "$suite.enc.raw" "$suite.enctamper.raw" <<'EOF'
import sys, os
pk = [bytes.fromhex(l.strip())[12:] for l in open(sys.argv[1]) if l.strip()]
whole = b"".join(pk)
ok = open(sys.argv[2], "rb").read() if os.path.exists(sys.argv[2]) else b""
t = open(sys.argv[3], "rb").read() if os.path.exists(sys.argv[3]) else b""
print("ok" if ok == whole and len(whole) > 0 else "bad", "ok" if t == b"".join(pk[:4] + pk[5:]) else "bad")
EOF
)
  if [ "${res% *}" = ok ]; then pass "senca→ffmpeg $suite: ffmpeg が復号した payload が元と一致"; else fail "senca→ffmpeg $suite"; fi
  if [ "${res#* }" = ok ]; then pass "senca→ffmpeg $suite: 改変した 1 packet だけを ffmpeg が捨てた"; else fail "senca→ffmpeg $suite: 改変"; fi
done

summary
