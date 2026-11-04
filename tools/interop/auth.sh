#!/usr/bin/env bash
# 利用者のログイン（ADR-0017）: 最初の管理者・パスワード・MFA（TOTP・回復コード）・失敗の上限・セッションの失効と期限・
# 利用者の管理と最後の管理者・権限の昇格の拒否・OIDC（試験用の IdP、state・nonce・PKCE の失敗、役割の対応、
# 自動で作る・作らない、テナントのクレーム）・回復の手順（reset-password）・3 台のクラスタ（コールバックが別のノード）。
# 使い方: tools/interop/auth.sh（KEEP=1 で作業場所を残す、NODE_BIN_PREBUILT=1 でビルドしない、SKIP_CLUSTER=1 でクラスタを飛ばす）
source "$(dirname "$0")/lib.sh"
CARGO=${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}
[ -n "${NODE_BIN_PREBUILT:-}" ] || (cd "$ROOT" && "$CARGO" build -q -p senca-node)
python3 -c "import cryptography" 2>/dev/null || { echo "SKIP: python3 の cryptography パッケージがありません"; exit 0; }
W=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; [ -n "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"; }
trap cleanup EXIT

# ---- 試験用の IdP（鍵は openssl で作る）
openssl genrsa -out "$W/idp.pem" 2048 2>/dev/null
IDP=$(port)
python3 -I "$ROOT/tools/interop/oidc_mock.py" "$IDP" "$W/idp.pem" >>"$W/idp.log" 2>&1 & PIDS+=($!)
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$IDP/.well-known/openid-configuration" >/dev/null && break; sleep 0.1; done
ISS="http://127.0.0.1:$IDP"
next() { curl -s -X POST "$ISS/_next" -d "$1" >/dev/null; }
echo "mock-client-secret" > "$W/client-secret"

HTTP=$(port)
BASE="http://127.0.0.1:$HTTP"
A="$BASE/api/v1"
cat > "$W/oidc.json" <<J
{"providers": [
  {"id": "mock", "display_name": "Mock IdP", "issuer": "$ISS", "client_id": "senca-confidential",
   "client_secret_file": "$W/client-secret", "allowed_email_domains": ["example.com"], "auto_provision": true,
   "role_mappings": [{"claim": "groups", "value": "senca-admins", "role": "admin"},
                     {"claim": "realm_access.roles", "value": "senca-ops", "role": "operator"}]},
  {"id": "mock-public", "display_name": "Mock (public client)", "issuer": "$ISS", "client_id": "senca-public",
   "auto_provision": false, "default_role": "viewer"},
  {"id": "mock-tenant", "issuer": "$ISS", "client_id": "senca-public", "tenant_claim": "org",
   "auto_provision": true, "default_role": "viewer", "require_email_verified": false}
]}
J
start_node() { # $1=データ置き場 $2=HTTP の番号 ...追加のオプション
  local d=$1 p=$2; shift 2
  "$NODE_BIN" --data-dir "$d" --http 127.0.0.1:$p --rtmp off --public-http-url "http://127.0.0.1:$p" "$@" >>"$d.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 100); do curl -sf "http://127.0.0.1:$p/readyz" >/dev/null && return 0; sleep 0.1; done
  return 1
}
start_node "$W/d" "$HTTP" --oidc-config "$W/oidc.json" --rate-limit-login 200 || { fail "node start"; cat "$W/d.log"; summary; exit 1; }
NODE_PID=${PIDS[-1]}
BOOT=$(cat "$W/d/control/bootstrap-admin-token")

# req METHOD PATH TOKEN [BODY] [追加の curl の引数...] → $CODE と $W/body
req() {
  local m=$1 p=$2 t=$3 b=${4:-}; shift 3; [ $# -gt 0 ] && shift
  local args=(-s -o "$W/body" -w '%{http_code}' -X "$m" "${REQ_BASE:-$A}$p" -H 'Content-Type: application/json')
  [ -n "$t" ] && args+=(-H "Authorization: Bearer $t")
  [ -n "$b" ] && args+=(-d "$b")
  CODE=$(curl "${args[@]}" "$@")
}
body() { json "$1" < "$W/body" 2>/dev/null || true; }
expect() { # $1=期待の状態 $2=説明
  if [ "$CODE" = "$1" ]; then pass "$2"; else fail "$2: HTTP $CODE $(head -c 300 "$W/body")"; fi
}
login() { # $1=username $2=password → $TOK（成功のとき）
  req POST /auth/login "" "{\"username\":\"$1\",\"password\":\"$2\"}"
  TOK=$( [ "$CODE" = 200 ] && body "['token']" 2>/dev/null || echo "")
}
totp() { python3 - "$@" <<'PY'
import base64, hashlib, hmac, struct, sys, time
s = sys.argv[1]
k = base64.b32decode(s + "=" * (-len(s) % 8))
c = int(time.time()) // 30 + (int(sys.argv[2]) if len(sys.argv) > 2 else 0)
h = hmac.new(k, struct.pack(">Q", c), hashlib.sha1).digest()
o = h[-1] & 15
print("%06d" % ((struct.unpack(">I", h[o:o + 4])[0] & 0x7FFFFFFF) % 1000000))
PY
}

echo "== 最初の管理者"
req GET /auth/providers ""
expect 200 "providers は認証なしで読める"
[ "$(body "['setup_required']")" = True ] && [ "$(body "['password']")" = True ] && [ "$(body "[\"oidc\"].__len__()")" = 3 ] \
  && pass "providers: setup_required=true・password・OIDC 3 つ" || fail "providers: $(cat "$W/body")"
[ "$(body "['oidc'][0]['start_url']")" = "/api/v1/auth/oidc/mock/start" ] && [ "$(body "['oidc'][0]['display_name']")" = "Mock IdP" ] \
  && pass "providers: start_url と表示名" || fail "providers oidc: $(cat "$W/body")"
ROOTPW='root password 123'
req POST /users "" "{\"username\":\"root\",\"password\":\"$ROOTPW\",\"roles\":[\"admin\"]}"
expect 401 "認証なしでは利用者を作れない"
req POST /users "$BOOT" "{\"username\":\"root\",\"password\":\"short\",\"roles\":[\"admin\"]}"
expect 400 "短いパスワードは 400"
req POST /users "$BOOT" "{\"username\":\"root\",\"password\":\"root\",\"roles\":[\"admin\"]}"
expect 400 "利用者名と同じパスワードは 400"
req POST /users "$BOOT" "{\"username\":\"root\",\"password\":\"$ROOTPW\",\"roles\":[\"admin\"],\"email\":\"root@example.com\"}"
expect 201 "最初のトークンで最初の管理者を作る"
ROOT_ID=$(body "['id']")
req GET /auth/providers ""
[ "$(body "['setup_required']")" = False ] && pass "利用者ができると setup_required=false" || fail "setup_required: $(cat "$W/body")"
req POST /users "$BOOT" "{\"username\":\"ROOT\",\"password\":\"$ROOTPW\",\"roles\":[\"viewer\"]}"
expect 409 "利用者名は大文字・小文字を区別せずに重ならない"

echo "== パスワードでのログイン"
login root "$ROOTPW"
[ "$CODE" = 200 ] && [[ "$TOK" == sencas_* ]] && pass "ログイン（sencas_ のセッション）" || fail "login: $CODE $(cat "$W/body")"
ROOT_TOK=$TOK
[ "$(body "['status']")" = ok ] && [ "$(body "['user']['username']")" = root ] && [ "$(body "['pending']")" = "[]" ] \
  && pass "ログインの応答に user・session・pending" || fail "login body: $(cat "$W/body")"
req GET /auth/me "$ROOT_TOK"
expect 200 "/auth/me"
[ "$(body "['subject']")" = "user:$ROOT_ID" ] && [ "$(body "['auth_method']")" = session ] && [ "$(body "['tenant']")" = default ] \
  && pass "/auth/me: subject=user:{id}・auth_method=session" || fail "me: $(cat "$W/body")"
req GET /streams "$ROOT_TOK"
expect 200 "セッションで管理 API を使える"
req GET /auth/me "$BOOT"
[ "$(body "['auth_method']")" = api_token ] && [ "$(body "['user']")" = None ] && pass "/auth/me は API トークンでも使える" || fail "me token: $(cat "$W/body")"
req POST /auth/login "" "{\"username\":\"root\",\"password\":\"wrong password!!\"}"; B1=$(body "['title']"); C1=$CODE
req POST /auth/login "" "{\"username\":\"nobody\",\"password\":\"wrong password!!\"}"; B2=$(body "['title']"); C2=$CODE
[ "$C1" = 401 ] && [ "$C2" = 401 ] && [ "$B1" = "$B2" ] && pass "パスワード違いと知らない利用者は同じ 401" || fail "enumeration: $C1/$C2 $B1/$B2"
t_known=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$A/auth/login" -H 'Content-Type: application/json' -d '{"username":"root","password":"wrong password!!"}')
t_unknown=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$A/auth/login" -H 'Content-Type: application/json' -d '{"username":"nobody2","password":"wrong password!!"}')
python3 -c "import sys; k,u=float(sys.argv[1]),float(sys.argv[2]); sys.exit(0 if u >= 0.3*k and k >= 0.005 else 1)" "$t_known" "$t_unknown" \
  && pass "知らない利用者でもハッシュの計算をする（${t_known}s / ${t_unknown}s）" || fail "timing: known=$t_known unknown=$t_unknown"

echo "== アカウントごとの失敗の上限"
for i in 1 2 3 4 5 6 7; do req POST /auth/login "" '{"username":"lockme","password":"wrong password!!"}'; done
req POST /auth/login "" '{"username":"lockme","password":"wrong password!!"}'
[ "$CODE" = 429 ] && [ "$(body "['details']['limit']")" = login_account ] && pass "7 回失敗したアカウントは 429（login_account）" || fail "lockout: $CODE $(cat "$W/body")"
req POST /auth/login "" "{\"username\":\"root\",\"password\":\"$ROOTPW\"}"
expect 200 "別のアカウントには影響しない"
req POST /users "$ROOT_TOK" '{"username":"lockme","password":"lockme password 1","roles":["viewer"]}'
expect 201 "利用者 lockme"
login lockme "lockme password 1"
[ "$CODE" = 429 ] && pass "待たせている間は正しいパスワードでも 429" || fail "lock with good pw: $CODE"
sleep 3.5
login lockme "lockme password 1"
[ "$CODE" = 200 ] && pass "待った後は入れる（成功で数え直す）" || fail "after wait: $CODE $(cat "$W/body")"

echo "== MFA（TOTP と回復コード）"
req POST /auth/mfa/totp/setup "$ROOT_TOK" '{}'
expect 400 "MFA の登録には今のパスワードが要る"
req POST /auth/mfa/totp/setup "$ROOT_TOK" '{"password":"wrong password!!"}'
expect 403 "パスワードが違えば 403"
req POST /auth/mfa/totp/setup "$ROOT_TOK" "{\"password\":\"$ROOTPW\"}"
expect 200 "MFA の登録を始める"
SECRET=$(body "['secret']"); ENROLL=$(body "['enrollment_token']"); URI=$(body "['otpauth_uri']")
[[ "$URI" == otpauth://totp/SENCA:root%40example.com\?secret=$SECRET\&issuer=SENCA* ]] && pass "otpauth:// の URI と base32 の秘密" || fail "uri: $URI"
req POST /auth/mfa/totp/confirm "$ROOT_TOK" "{\"enrollment_token\":\"$ENROLL\",\"code\":\"000000\"}"
expect 400 "違う確認コードでは有効にならない"
req POST /auth/mfa/totp/confirm "$ROOT_TOK" "{\"enrollment_token\":\"$ENROLL\",\"code\":\"$(totp "$SECRET")\"}"
expect 200 "確認コードで MFA を有効にする"
RC1=$(body "['recovery_codes'][0]"); NRC=$(body "['recovery_codes'].__len__()")
[ "$NRC" = 10 ] && [ "$(body "['user']['mfa']['enabled']")" = True ] && pass "回復コード 10 個を 1 度だけ返す" || fail "confirm: $(cat "$W/body")"
req POST /auth/mfa/totp/setup "$ROOT_TOK" "{\"password\":\"$ROOTPW\"}"
expect 409 "有効な MFA の上から登録し直せない"
login root "$ROOTPW"
[ "$CODE" = 200 ] && [ "$(body "['status']")" = mfa_required ] && [ -z "$TOK" ] && pass "MFA の利用者はパスワードだけでは mfa_required" || fail "mfa required: $(cat "$W/body")"
MT=$(body "['mfa_token']")
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"code\":\"123456\"}"
expect 401 "違う TOTP は 401"
CODE1=$(totp "$SECRET" 1)
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"code\":\"$CODE1\"}"
expect 200 "TOTP でログイン"
[ "$(body "['session']['mfa']")" = True ] && pass "セッションに mfa=true" || fail "session mfa: $(cat "$W/body")"
MFA_TOK=$(body "['token']")
login root "$ROOTPW"; MT=$(body "['mfa_token']")
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"code\":\"$CODE1\"}"
expect 401 "同じ TOTP のコードは 2 度使えない"
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"recovery_code\":\"$RC1\"}"
expect 200 "回復コードでログイン"
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"recovery_code\":\"$RC1\"}"
expect 401 "同じ回復コードは 2 度使えない"
req POST /auth/login/mfa "" "{\"mfa_token\":\"x$MT\",\"code\":\"$CODE1\"}"
expect 401 "壊れた mfa_token は 401"
req GET /auth/me "$MFA_TOK"
[ "$(body "['user']['mfa']['recovery_codes_remaining']")" = 9 ] && pass "回復コードの残りは 9" || fail "remaining: $(cat "$W/body")"
req POST /auth/mfa/recovery-codes "$MFA_TOK" "{\"password\":\"$ROOTPW\"}"
expect 200 "回復コードを作り直す"
RC2=$(body "['recovery_codes'][1]")

echo "== パスワードの変更とセッションの失効"
req POST /auth/password "$MFA_TOK" "{\"current_password\":\"wrong password!!\",\"new_password\":\"another password 456\"}"
expect 403 "今のパスワードが違えば 403"
req POST /auth/password "$MFA_TOK" "{\"current_password\":\"$ROOTPW\",\"new_password\":\"short\"}"
expect 400 "弱い新しいパスワードは 400"
req GET /auth/sessions "$MFA_TOK"
expect 200 "自分のセッションの一覧"
NS=$(body "['items'].__len__()"); CUR=$(python3 -c "import json,sys; print(sum(1 for s in json.load(open(sys.argv[1]))['items'] if s['current']))" "$W/body")
[ "$NS" -ge 3 ] && [ "$CUR" = 1 ] && pass "セッションの一覧（$NS 件、今のものに current）" || fail "sessions: $(cat "$W/body")"
ROOTPW2='another password 456'
req POST /auth/password "$MFA_TOK" "{\"current_password\":\"$ROOTPW\",\"new_password\":\"$ROOTPW2\"}"
expect 204 "パスワードを変える"
req GET /auth/me "$ROOT_TOK"
expect 401 "パスワードを変えると、ほかのセッションは失効する"
req GET /auth/me "$MFA_TOK"
expect 200 "変えたセッションは使い続けられる"
login root "$ROOTPW"
expect 401 "古いパスワードでは入れない"
login root "$ROOTPW2"; MT=$(body "['mfa_token']")
req POST /auth/login/mfa "" "{\"mfa_token\":\"$MT\",\"recovery_code\":\"$RC2\"}"
expect 200 "新しいパスワードと作り直した回復コードで入る"
SECOND=$(body "['token']"); SECOND_ID=$(body "['session']['id']")
req DELETE "/auth/sessions/$SECOND_ID" "$MFA_TOK"
expect 204 "自分の別のセッションを失効させる"
req GET /auth/me "$SECOND"
expect 401 "失効させたセッションは 401"
req DELETE "/auth/sessions/nosuch" "$MFA_TOK"
expect 404 "知らないセッションは 404"
ADMIN=$MFA_TOK

echo "== 利用者の管理"
req POST /users "$ADMIN" '{"username":"op","password":"operator password 1","roles":["operator"],"must_change_password":true,"email":"op@example.com"}'
expect 201 "operator を作る（次のログインでパスワードの変更）"
OP_ID=$(body "['id']")
login op "operator password 1"
OP=$TOK
[ "$(body "['pending']")" = "['password_change']" ] && pass "仮のパスワードのセッションは pending=[password_change]" || fail "pending: $(cat "$W/body")"
req GET /streams "$OP"
[ "$CODE" = 403 ] && [ "$(body "['details']['reason']")" = pending ] && pass "パスワードを変えるまでほかの API は 403" || fail "pending gate: $CODE $(cat "$W/body")"
req POST /auth/password "$OP" '{"current_password":"operator password 1","new_password":"operator password 2"}'
expect 204 "仮のパスワードを変える"
req GET /streams "$OP"
expect 200 "変えた後は使える"
req GET /users "$OP"
expect 403 "operator は利用者を管理できない"
req POST /users "$OP" '{"username":"x","roles":["viewer"]}'
expect 403 "operator は利用者を作れない"
# 権限の昇格: tenant:admin だけのトークンは node:admin を渡せない・node:admin の利用者を扱えない
req POST /tokens "$BOOT" '{"name":"tadmin","scopes":["tenant:admin"]}'
TADMIN=$(body "['token']")
req POST /users "$TADMIN" '{"username":"na","roles":["viewer"],"scopes":["node:admin"]}'
expect 403 "持っていない権限（node:admin）は渡せない"
req POST /users "$BOOT" '{"username":"nodeop","password":"node operator 1","roles":["viewer"],"scopes":["node:admin"]}'
expect 201 "node:admin を持つ主体は node:admin を渡せる"
NODEOP_ID=$(body "['id']")
req POST "/users/$NODEOP_ID/password" "$ADMIN" '{}'
expect 403 "自分より強い利用者のパスワードは再設定できない"
req DELETE "/users/$NODEOP_ID" "$TADMIN"
expect 403 "自分より強い利用者は消せない"
req GET "/users?limit=2" "$ADMIN"
expect 200 "利用者の一覧（limit）"
NEXT=$(body "['next_cursor']")
req GET "/users?limit=2&cursor=$NEXT" "$ADMIN"
expect 200 "利用者の一覧（cursor）"
req GET "/users/$OP_ID" "$ADMIN"
G=$(body "['generation']")
req PATCH "/users/$OP_ID" "$ADMIN" '{"roles":["viewer"]}'
expect 428 "PATCH は If-Match が要る"
req PATCH "/users/$OP_ID" "$ADMIN" '{"roles":["viewer"]}' -H 'If-Match: "999"'
expect 412 "古い If-Match は 412"
req PATCH "/users/$OP_ID" "$ADMIN" '{"roles":["viewer"],"display_name":"Operator"}' -H "If-Match: \"$G\""
expect 200 "役割を変える"
[ "$(body "['roles']")" = "['viewer']" ] && [ "$(body "['display_name']")" = Operator ] && pass "役割と表示名が変わる" || fail "patch: $(cat "$W/body")"
req GET /auth/me "$OP"
expect 401 "役割を変えるとセッションは失効する"
req GET "/users/$OP_ID" "$ADMIN"; G=$(body "['generation']")
req PATCH "/users/$OP_ID" "$ADMIN" '{"disabled":true}' -H "If-Match: \"$G\""
expect 200 "無効にする"
login op "operator password 2"
expect 401 "無効な利用者は入れない（同じ 401）"
req GET "/users/$OP_ID" "$ADMIN"; G=$(body "['generation']")
req PATCH "/users/$OP_ID" "$ADMIN" '{"disabled":false}' -H "If-Match: \"$G\""
expect 200 "有効に戻す"
req POST "/users/$OP_ID/password" "$ADMIN" '{}'
expect 200 "管理者がパスワードを再設定する"
TMP=$(body "['temporary_password']")
[ ${#TMP} = 20 ] && [ "$(body "['user']['must_change_password']")" = True ] && pass "仮のパスワード（20 文字、must_change_password）" || fail "reset: $(cat "$W/body")"
login op "$TMP"
[ "$CODE" = 200 ] && [ "$(body "['pending']")" = "['password_change']" ] && pass "仮のパスワードで入ると変更を求められる" || fail "temp login: $CODE $(cat "$W/body")"
OP=$TOK
req GET "/users/$OP_ID/sessions" "$ADMIN"
[ "$CODE" = 200 ] && [ "$(body "['items'].__len__()")" = 1 ] && pass "利用者のセッションの一覧" || fail "user sessions: $(cat "$W/body")"
req DELETE "/users/$OP_ID/sessions" "$ADMIN"
expect 204 "利用者のセッションをすべて失効させる"
req GET /auth/me "$OP"
expect 401 "失効したセッションは 401"
req DELETE "/users/$ROOT_ID/mfa" "$ADMIN"
expect 204 "管理者が MFA を外す"
login root "$ROOTPW2"
[ "$CODE" = 200 ] && [ "$(body "['status']")" = ok ] && pass "MFA を外すとパスワードだけで入れる" || fail "after mfa reset: $(cat "$W/body")"
ADMIN=$TOK

echo "== 最後の管理者"
req DELETE "/users/$ROOT_ID" "$ADMIN"
expect 409 "最後の管理者は消せない"
req GET "/users/$ROOT_ID" "$ADMIN"; G=$(body "['generation']")
req PATCH "/users/$ROOT_ID" "$ADMIN" '{"disabled":true}' -H "If-Match: \"$G\""
expect 409 "最後の管理者は無効にできない"
req PATCH "/users/$ROOT_ID" "$ADMIN" '{"roles":["operator"]}' -H "If-Match: \"$G\""
expect 409 "最後の管理者は降格できない"
req POST /users "$ADMIN" '{"username":"root2","password":"second admin pw 1","roles":["admin"]}'
expect 201 "2 人目の管理者"
ROOT2_ID=$(body "['id']")
req PATCH "/users/$ROOT_ID" "$ADMIN" '{"roles":["operator"]}' -H "If-Match: \"$G\""
expect 200 "2 人目がいれば降格できる"
login root2 "second admin pw 1"; ADMIN=$TOK
req GET "/users/$ROOT_ID" "$ADMIN"; G=$(body "['generation']")
req PATCH "/users/$ROOT_ID" "$ADMIN" '{"roles":["admin"]}' -H "If-Match: \"$G\""
expect 200 "管理者に戻す"
req DELETE "/users/$ROOT2_ID" "$ADMIN"
expect 204 "管理者が 2 人いれば 1 人は消せる"
req GET /auth/me "$ADMIN"
expect 401 "消した利用者のセッションは 401"
login root "$ROOTPW2"; ADMIN=$TOK

echo "== SSE の利用者の出来事"
req POST /tokens "$BOOT" '{"name":"ro","scopes":["stream:read"]}'; RO=$(body "['token']")
( curl -s -N --max-time 4 -H "Authorization: Bearer $ADMIN" "$A/events" > "$W/sse-admin" & curl -s -N --max-time 4 -H "Authorization: Bearer $RO" "$A/events" > "$W/sse-ro" & wait ) &
SSE=$!
sleep 1
req POST /users "$ADMIN" '{"username":"sseuser","roles":["viewer"]}'
SSE_ID=$(body "['id']")
req DELETE "/users/$SSE_ID" "$ADMIN"
wait $SSE || true
grep -q "event: user.created" "$W/sse-admin" && grep -q "event: user.deleted" "$W/sse-admin" && pass "SSE: 管理者には user.created・user.deleted" || fail "sse admin: $(head -c 400 "$W/sse-admin")"
grep -q "event: user\." "$W/sse-ro" && fail "SSE: stream:read に user.* が流れた" || pass "SSE: stream:read には user.* を流さない"

echo "== OIDC"
# oidc PROVIDER JAR [return_to] [callback の state を書き換える] → $LOC3（管理画面への戻り先）、$OC（code）、$OE（error）
oidc() {
  local prov=$1 jar=$2 rt=${3:-} tamper=${4:-}
  local q=""; [ -n "$rt" ] && q="?return_to=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1],safe=""))' "$rt")"
  local start=${OIDC_START:-$BASE}
  LOC1=$(curl -s -o /dev/null -b "$jar" -c "$jar" -w '%{redirect_url}' "$start/api/v1/auth/oidc/$prov/start$q")
  local loc2; loc2=$(curl -s -o /dev/null -w '%{redirect_url}' "$LOC1")
  [ -n "$tamper" ] && loc2=$(echo "$loc2" | sed 's/state=[^&]*/state=tampered/')
  [ -n "${NO_COOKIE:-}" ] && jar=/dev/null
  LOC3=$(curl -s -o /dev/null -b "$jar" -c "$jar" -w '%{redirect_url}' "$loc2")
  OC=$(echo "$LOC3" | sed -n 's/.*[?&]code=\([^&]*\).*/\1/p'); OE=$(echo "$LOC3" | sed -n 's/.*[?&]error=\([^&]*\).*/\1/p')
}
exchange() { # $1=JAR $2=code [$3=base]
  CODE=$(curl -s -o "$W/body" -w '%{http_code}' -b "$1" -c "$1" -X POST "${3:-$BASE}/api/v1/auth/exchange" -H 'Content-Type: application/json' -d "{\"code\":\"$2\"}")
}
J1="$W/jar1"
next '{"claims":{"sub":"alice-1","email":"alice@example.com","email_verified":true,"name":"Alice","preferred_username":"alice","realm_access":{"roles":["senca-ops"]}}}'
oidc mock "$J1" "/console/#/streams"
[[ "$LOC1" == "$ISS/authorize?"*code_challenge_method=S256* ]] && pass "start は IdP へ 302（PKCE S256）" || fail "start: $LOC1"
grep -q "senca_oidc" "$J1" && grep -q "#HttpOnly_" "$J1" && pass "途中の値は HttpOnly のクッキー" || fail "cookie: $(cat "$J1")"
[[ "$LOC3" == "$BASE/console/#/auth/complete?code="* ]] && [ -n "$OC" ] && pass "コールバックは管理画面へ一度きりのコードを渡す" || fail "callback: $LOC3"
exchange "$J1" "$OC"
expect 200 "コードをセッションに替える"
ALICE=$(body "['token']"); ALICE_ID=$(body "['user']['id']")
[ "$(body "['return_to']")" = "/console/#/streams" ] && [ "$(body "['session']['method']")" = oidc ] && [ "$(body "['session']['provider']")" = mock ] \
  && pass "exchange の応答に return_to・method=oidc・provider" || fail "exchange body: $(cat "$W/body")"
[ "$(body "['user']['roles']")" = "['operator']" ] && [ "$(body "['user']['has_password']")" = False ] && [ "$(body "['user']['username']")" = alice ] \
  && pass "自動で作った利用者（realm_access.roles → operator、パスワード無し）" || fail "provisioned: $(cat "$W/body")"
exchange "$J1" "$OC"
expect 401 "同じコードは 2 度使えない"
req GET /streams "$ALICE"
expect 200 "OIDC のセッションで管理 API を使える"
login alice "anything at all 1"
expect 401 "OIDC だけの利用者はパスワードでは入れない"
req POST /auth/mfa/totp/setup "$ALICE" '{"password":"x"}'
expect 409 "OIDC の利用者は SENCA の MFA を使わない"
# 役割の対応: groups に senca-admins → admin（ログインのたびに決め直す）
next '{"claims":{"sub":"alice-1","email":"alice@example.com","email_verified":true,"groups":["senca-admins"]}}'
oidc mock "$J1"; exchange "$J1" "$OC"
[ "$CODE" = 200 ] && [ "$(body "['user']['roles']")" = "['admin']" ] && pass "役割の対応で admin になる" || fail "role sync: $CODE $(cat "$W/body")"
req GET /auth/me "$ALICE"
expect 401 "役割が変わると、前のセッションは失効する"
# メールアドレスのドメイン・確認
next '{"claims":{"sub":"bob-1","email":"bob@evil.example","email_verified":true,"groups":["senca-admins"]}}'
oidc mock "$J1"; [ "$OE" = email_domain_not_allowed ] && pass "許していないドメインは email_domain_not_allowed" || fail "domain: $LOC3"
next '{"claims":{"sub":"bob-2","email":"bob@example.com","email_verified":false,"groups":["senca-admins"]}}'
oidc mock "$J1"; [ "$OE" = email_not_verified ] && pass "確認されていないメールアドレスは email_not_verified" || fail "verified: $LOC3"
next '{"claims":{"sub":"bob-3","email":"bob@example.com","email_verified":true}}'
oidc mock "$J1"; [ "$OE" = no_role ] && pass "役割の対応に当たらず default_role も無ければ no_role" || fail "no_role: $LOC3"
# 自動で作らない IdP（public client）: 知らない利用者は no_account、同じメールアドレスの利用者があれば結び付ける
J2="$W/jar2"
next '{"claims":{"sub":"carol-1","email":"carol@example.com","email_verified":true}}'
oidc mock-public "$J2"; [ "$OE" = no_account ] && pass "自動で作らない IdP の知らない利用者は no_account" || fail "no_account: $LOC3"
req POST /users "$ADMIN" '{"username":"carol","email":"Carol@example.com","roles":["operator"]}'
expect 201 "carol を先に作る（パスワード無し）"
CAROL_ID=$(body "['id']")
next '{"claims":{"sub":"carol-1","email":"carol@example.com","email_verified":true}}'
oidc mock-public "$J2"; exchange "$J2" "$OC"
[ "$CODE" = 200 ] && [ "$(body "['user']['id']")" = "$CAROL_ID" ] && [ "$(body "['user']['roles']")" = "['operator']" ] \
  && [ "$(body "['user']['oidc'][0]['subject']")" = carol-1 ] && pass "同じメールアドレスの利用者に結び付ける（public client・役割はそのまま）" || fail "link: $CODE $(cat "$W/body")"
next '{"claims":{"sub":"carol-1","email":"changed@example.com"}}'
oidc mock-public "$J2"; exchange "$J2" "$OC"
[ "$CODE" = 200 ] && [ "$(body "['user']['id']")" = "$CAROL_ID" ] && pass "結び付いた後は sub で決まる（メールアドレスが変わっても同じ利用者）" || fail "by sub: $(cat "$W/body")"
req GET "/users/$CAROL_ID" "$ADMIN"; G=$(body "['generation']")
req PATCH "/users/$CAROL_ID" "$ADMIN" '{"disabled":true}' -H "If-Match: \"$G\""
next '{"claims":{"sub":"carol-1"}}'
oidc mock-public "$J2"; [ "$OE" = account_disabled ] && pass "無効な利用者は account_disabled" || fail "disabled: $LOC3"
# テナントをクレームから決める
J3="$W/jar3"
next '{"claims":{"sub":"dave-1","org":"acme","email":"dave@acme.example"}}'
oidc mock-tenant "$J3"; exchange "$J3" "$OC"
[ "$CODE" = 200 ] && [ "$(body "['tenant']")" = acme ] && [ "$(body "['user']['roles']")" = "['viewer']" ] && pass "テナントはクレーム（org=acme）、default_role=viewer" || fail "tenant: $(cat "$W/body")"
DAVE_ID=$(body "['user']['id']")
req GET "/users/$DAVE_ID" "$ADMIN"
expect 404 "別のテナントの利用者は 404"
next '{"claims":{"sub":"eve-1","org":"bad tenant!"}}'
oidc mock-tenant "$J3"; [ "$OE" = tenant_not_allowed ] && pass "テナントのクレームの形が違えば tenant_not_allowed" || fail "tenant bad: $LOC3"
# 失敗: state・クッキー・nonce・aud・期限・署名・alg・iss・kid・PKCE・IdP の拒否
next '{"claims":{"sub":"alice-1","email":"alice@example.com","email_verified":true,"groups":["senca-admins"]}}'
oidc mock "$J1" "" tamper; [ "$OE" = invalid_state ] && pass "state が違えば invalid_state" || fail "state: $LOC3"
next '{"claims":{"sub":"alice-1","email":"alice@example.com","email_verified":true,"groups":["senca-admins"]}}'
NO_COOKIE=1 oidc mock "$J1"; [ "$OE" = invalid_state ] && pass "途中のクッキーが無ければ invalid_state" || fail "no cookie: $LOC3"
for f in bad_nonce no_nonce bad_aud multi_aud expired bad_sig alg_none bad_iss unknown_kid; do
  [ "$f" = unknown_kid ] && sleep 6  # JWKS を取り直せる間隔（5 秒）を空ける
  next "{\"claims\":{\"sub\":\"alice-1\",\"email\":\"alice@example.com\",\"email_verified\":true,\"groups\":[\"senca-admins\"]},\"fault\":\"$f\"}"
  oidc mock "$J1"; [ "$OE" = invalid_id_token ] && pass "ID トークンの $f は invalid_id_token" || fail "fault $f: $LOC3"
done
JW=$(curl -s "$ISS/_stats" | json "['jwks']")
[ "$JW" -ge 2 ] && pass "知らない kid で JWKS を取り直す（$JW 回）" || fail "jwks refresh: $JW"
next '{"claims":{"sub":"alice-1"},"fault":"pkce_mismatch"}'
oidc mock "$J1"; [ "$OE" = token_exchange_failed ] && pass "PKCE の verifier が合わなければ token_exchange_failed" || fail "pkce: $LOC3"
next '{"claims":{"sub":"alice-1"},"fault":"deny"}'
oidc mock "$J1"; [ "$OE" = access_denied ] && pass "IdP が断れば access_denied" || fail "deny: $LOC3"
grep -q "senca_oidc" "$J1" && fail "失敗したら途中のクッキーを消す: $(cat "$J1")" || pass "失敗したら途中のクッキーを消す"
# 別のブラウザーのコードは使えない（ログインの CSRF を防ぐ）
next '{"claims":{"sub":"alice-1","email":"alice@example.com","email_verified":true,"groups":["senca-admins"]}}'
oidc mock "$J1"; exchange "$W/jar-other" "$OC"
expect 401 "別のブラウザー（クッキーが違う）ではコードを替えられない"
exchange "$J1" "$OC"
expect 200 "始めたブラウザーなら替えられる"
c=$(curl -s -o /dev/null -w '%{http_code}' "$A/auth/oidc/mock/start?return_to=https://evil.example/")
[ "$c" = 400 ] && pass "return_to は管理画面のパスだけ（ほかは 400）" || fail "return_to: $c"
c=$(curl -s -o /dev/null -w '%{http_code}' "$A/auth/oidc/nosuch/start")
[ "$c" = 404 ] && pass "知らない IdP は 404" || fail "unknown provider: $c"
AQ=$(curl -s "$ISS/_stats" | python3 -c "import json,sys; q=json.load(sys.stdin)['authorize']; print(q['redirect_uri'], q['scope'], len(q['nonce']), len(q['state']))")
[ "$AQ" = "$BASE/api/v1/auth/oidc/mock/callback openid email profile 43 43" ] && pass "認可の要求: redirect_uri・scope・nonce・state" || fail "authorize query: $AQ"

echo "== 回復の手順（reset-password）"
out=$("$NODE_BIN" reset-password --api "$BASE" --data-dir "$W/d" --username root 2>&1) && rc=0 || rc=$?
NEWPW=$(echo "$out" | sed -n 's/^temporary password for root: //p')
[ $rc = 0 ] && [ ${#NEWPW} = 20 ] && pass "reset-password が仮のパスワードを出す" || fail "reset-password: $out"
login root "$NEWPW"
[ "$CODE" = 200 ] && [ "$(body "['pending']")" = "['password_change']" ] && pass "仮のパスワードで入れる（変更を求める）" || fail "recovered login: $(cat "$W/body")"
out=$("$NODE_BIN" reset-password --api "$BASE" --data-dir "$W/d" --username newadmin --create-admin 2>&1) && rc=0 || rc=$?
NEWPW=$(echo "$out" | sed -n 's/^temporary password for newadmin: //p')
login newadmin "$NEWPW"
[ "$CODE" = 200 ] && [ "$(body "['user']['roles']")" = "['admin']" ] && pass "--create-admin で管理者を作る" || fail "create-admin: $out $(cat "$W/body")"

echo "== IP ごとのログインの失敗の上限"
kill "$NODE_PID"; wait "$NODE_PID" 2>/dev/null || true
start_node "$W/d" "$HTTP" --oidc-config "$W/oidc.json" --rate-limit-login 5 --recovery-admin-token || fail "restart"
NODE_PID=${PIDS[-1]}
[ -s "$W/d/control/recovery-admin-token" ] && [ "$(stat -c %a "$W/d/control/recovery-admin-token")" = 600 ] && pass "--recovery-admin-token で回復用のトークン（0600）" || fail "recovery token"
REC=$(cat "$W/d/control/recovery-admin-token")
req GET /users "$REC"
expect 200 "回復用のトークンで利用者を管理できる"
for i in $(seq 5); do req POST /auth/login "" "{\"username\":\"u$i\",\"password\":\"wrong password!!\"}"; done
req POST /auth/login "" "{\"username\":\"newadmin\",\"password\":\"$NEWPW\"}"
[ "$CODE" = 429 ] && [ "$(body "['details']['limit']")" = login ] && pass "IP ごとの失敗を使い切ると 429（login）" || fail "ip limit: $CODE $(cat "$W/body")"
kill "$NODE_PID"; wait "$NODE_PID" 2>/dev/null || true

echo "== セッションの期限（待ち時間 10 秒・寿命 20 秒）"
H2=$(port)
start_node "$W/e" "$H2" --auth-session-idle 10 --auth-session-lifetime 20 || fail "node e"
REQ_BASE="http://127.0.0.1:$H2/api/v1"
req POST /users "$(cat "$W/e/control/bootstrap-admin-token")" '{"username":"short","password":"short lived pw 1","roles":["viewer"]}'
login short "short lived pw 1"; S=$TOK
ok=1
for t in 4 8 12 16; do sleep 4; req GET /auth/me "$S"; [ "$CODE" = 200 ] || { ok=0; fail "使っている間は続く（${t}s: $CODE）"; break; }; done
[ $ok = 1 ] && pass "使っている間は待ち時間を超えても続く（最後に使った時刻を進める）"
sleep 5; req GET /auth/me "$S"
expect 401 "寿命（20 秒）を過ぎると失効する"
login short "short lived pw 1"; S=$TOK
sleep 11; req GET /auth/me "$S"
expect 401 "待ち時間（10 秒）使わなければ失効する"
unset REQ_BASE

echo "== 秘密がログに出ない"
if grep -h -E "sencas_|root password 123|another password 456|$SECRET|mock-client-secret" "$W"/d.log "$W"/d/audit.log "$W"/e.log >/dev/null 2>&1; then
  fail "パスワード・セッション・TOTP の秘密・クライアントシークレットがログに出た"
else
  pass "パスワード・セッション・TOTP の秘密・クライアントシークレットはログ・監査ログに出ない"
fi
grep -q '"action":"auth.login".*"outcome":"failure"' "$W/d/audit.log" && grep -q '"action":"auth.login".*"method":"oidc:mock".*"outcome":"success"' "$W/d/audit.log" \
  && grep -q '"action":"auth.mfa.enable"' "$W/d/audit.log" && grep -q '"action":"user.password.reset"' "$W/d/audit.log" \
  && pass "監査ログにログイン（成功・失敗・方法）・MFA・パスワードの再設定" || fail "audit log: $(tail -3 "$W/d/audit.log")"

if [ -z "${SKIP_CLUSTER:-}" ]; then
  echo "== 3 台のクラスタ（コールバックが別のノードに届く）"
  echo "0123456789abcdef-cluster-secret" > "$W/secret"
  declare -A CH CR CL
  for i in 1 2 3; do CH[$i]=$(port); CR[$i]=$(port); CL[$i]=$(port); done
  sed "s#\"issuer\": \"$ISS\", \"client_id\": \"senca-confidential\"#\"issuer\": \"$ISS\", \"client_id\": \"senca-confidential\", \"redirect_url\": \"http://127.0.0.1:${CH[2]}/api/v1/auth/oidc/mock/callback\"#" "$W/oidc.json" > "$W/oidc-cluster.json"
  for i in 1 2 3; do
    cat > "$W/c$i.json" <<J
{"node_id": $i, "listen": "127.0.0.1:${CR[$i]}", "relay_listen": "127.0.0.1:${CL[$i]}", "secret_file": "$W/secret",
 "peers": [{"id":1,"addr":"127.0.0.1:${CR[1]}","relay":"127.0.0.1:${CL[1]}","api":"http://127.0.0.1:${CH[1]}"},
           {"id":2,"addr":"127.0.0.1:${CR[2]}","relay":"127.0.0.1:${CL[2]}","api":"http://127.0.0.1:${CH[2]}"},
           {"id":3,"addr":"127.0.0.1:${CR[3]}","relay":"127.0.0.1:${CL[3]}","api":"http://127.0.0.1:${CH[3]}"}]}
J
  done
  for i in 1 2 3; do
    "$NODE_BIN" --data-dir "$W/n$i" --http 127.0.0.1:${CH[$i]} --rtmp off --public-http-url "http://127.0.0.1:${CH[$i]}" \
      --cluster-config "$W/c$i.json" --node-name n$i --oidc-config "$W/oidc-cluster.json" >>"$W/n$i.log" 2>&1 & PIDS+=($!)
  done
  for i in 1 2 3; do ok=0; for _ in $(seq 150); do curl -sf "http://127.0.0.1:${CH[$i]}/readyz" >/dev/null && { ok=1; break; }; sleep 0.2; done
    [ $ok = 1 ] || { fail "cluster node $i not ready"; tail -20 "$W/n$i.log"; }; done
  CB=$(cat "$W/n1/control/bootstrap-admin-token")
  REQ_BASE="http://127.0.0.1:${CH[1]}/api/v1"
  req POST /users "$CB" '{"username":"cadmin","password":"cluster admin pw","roles":["admin"]}'
  expect 201 "node 1 で管理者を作る"
  REQ_BASE="http://127.0.0.1:${CH[3]}/api/v1"
  ok=0; for _ in $(seq 20); do login cadmin "cluster admin pw"; [ "$CODE" = 200 ] && { ok=1; break; }; sleep 0.2; done
  [ $ok = 1 ] && pass "node 3 でパスワードのログイン" || fail "cluster login: $CODE $(cat "$W/body")"
  CS=$TOK
  for i in 1 2; do REQ_BASE="http://127.0.0.1:${CH[$i]}/api/v1"; req GET /auth/me "$CS"; expect 200 "node 3 のセッションがすぐに node $i で使える"; done
  JC="$W/jar-cluster"
  next '{"claims":{"sub":"zed-1","email":"zed@example.com","email_verified":true,"groups":["senca-admins"]}}'
  OIDC_START="http://127.0.0.1:${CH[1]}" oidc mock "$JC" "/console/"
  [[ "$LOC3" == "http://127.0.0.1:${CH[2]}/console/#/auth/complete?code="* ]] && pass "node 1 で始め、コールバックは node 2 が受ける" || fail "cluster callback: $LOC3 $(tail -3 "$W/n2.log")"
  exchange "$JC" "$OC" "http://127.0.0.1:${CH[3]}"
  [ "$CODE" = 200 ] && pass "node 3 でコードをセッションに替える" || fail "cluster exchange: $CODE $(cat "$W/body")"
  ZS=$(body "['token']")
  REQ_BASE="http://127.0.0.1:${CH[1]}/api/v1"; req GET /auth/me "$ZS"
  [ "$CODE" = 200 ] && [ "$(body "['user']['roles']")" = "['admin']" ] && pass "OIDC のセッションが node 1 でも使える" || fail "cluster oidc session: $(cat "$W/body")"
  REQ_BASE="http://127.0.0.1:${CH[2]}/api/v1"; req POST /auth/logout "$ZS"
  expect 204 "node 2 でログアウト"
  sleep 0.5
  for i in 1 3; do REQ_BASE="http://127.0.0.1:${CH[$i]}/api/v1"; req GET /auth/me "$ZS"; expect 401 "ログアウトしたセッションは node $i でも 401"; done
  unset REQ_BASE
fi
summary
