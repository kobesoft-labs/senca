# 12. セキュリティ

認証と権限、送り手・視聴者の資格（stream key・署名付き URL・参加トークン）、秘密値の保管、外へ出る接続の制限、
クラスタの mTLS、codec worker のサンドボックス、そしてまだ独立したレビューを受けていない部分。

- 関係するページ: [3. 設定](03-configuration.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[10. リアルタイムの部屋](10-rooms.md)、[13. 運用](13-operations.md)、[15. 管理 API と SDK](15-api-and-sdks.md)
- 設計の理由: [ADR-0002 依存と暗号の部品](../../adr/0002-dependencies-and-crypto.md)、[ADR-0007 入力・出力・秘密値と外へ出る接続の制限](../../adr/0007-inputs-outputs-egress.md)、[ADR-0009 分散の制御面](../../adr/0009-cluster.md)

> SENCA は v0.0.0（実験的）。TLS・DTLS・SRTP・SRT の暗号・QUIC のプロトコル部分と、認可・分散の所有権の実装は、
> まだ独立したレビューを受けていない（[12.13](#1213-まだ独立したレビューを受けていないもの)）。

## 12.1 管理 API の認証

管理 API（`/api/v1/...`）と SSE（`/api/v1/events`）は `Authorization: Bearer <値>` で認証する。

- 値が `senca_` で始まれば **API トークン**、それ以外は **JWT**（`--jwks` を指定したときだけ）。
- 失敗は 401（`WWW-Authenticate: Bearer`）。理由（未知のトークン・期限切れ・署名の誤りなど）は応答に出さず、
  ノードのログに `auth failed: <理由> request_id=...` として残す。
- 認証なしで答えるのは `/healthz`・`/readyz`・管理画面と送出ページの静的ファイル・署名付きの再生 URL（`/s/...`）。
  WHIP は stream key、WHEP は署名、部屋のシグナリングは参加トークンで認める。

### API トークン

- 形: `senca_` + 32 バイトの乱数の base64url（43 文字）。サーバーには SHA-256 だけを残し、照合は全件を定数時間で比べる。
- 値が返るのは作成の応答（`POST /api/v1/tokens`、201）だけ。一覧・取得では返らない。
- 作成の本文: `{"name": "...", "scopes": ["..."], "expires_at": "RFC 3339"}`（`expires_at` は省略可。過去は不可）。
- 作るには `tenant:admin` が要り、**自分が持っている権限しか渡せない**。作ったトークンは作った者と同じテナントに属する。
- テナントあたり 100 本まで。テナントの最後の `tenant:admin` のトークンは消せない（409）。

### 権限（scope）

| 権限 | できること |
|---|---|
| `stream:read` | stream・予約・入力・出力・パイプライン・部屋・メディアパイプラインの参照、静止画、SSE の購読 |
| `stream:write` | stream の作成・変更・削除・鍵の作り直し、録画の開始・停止・変更（保持期間・法的保全）、予約・入力・出力・パイプライン・部屋・メディアパイプラインの変更、部屋の参加トークンの発行 |
| `publish` | （予約。今の API で要求する操作は無い） |
| `subscribe` | ライブの再生 URL の発行 |
| `recording:read` | 録画の参照、録画の再生 URL・サムネイル・MP4 の書き出し |
| `recording:delete` | 録画の削除 |
| `tenant:admin` | `node:admin` 以外のすべて。加えてトークン・秘密値の管理 |
| `node:admin` | テナントの外の運用（`/nodes`・`/grid`・`/devices`、排出、装置の停止・再開、ノード・グリッドのイベント） |

操作ごとの権限は [15. 管理 API と SDK](15-api-and-sdks.md) と OpenAPI（[docs/api/openapi.yaml](../../api/openapi.yaml)）。

### テナント

すべての資源はテナントに属し、他のテナントの資源は見えない（404）。最初の管理者トークンのテナントは `default`。
API トークンは作った者のテナントにしか作れないので、**別のテナントを作るには JWT を使う**（テナントは JWT のクレームで決まる）。

### 最初の管理者トークン

初回の起動で `data_dir/control/bootstrap-admin-token`（0600）に書かれる（`tenant:admin`・`node:admin`、期限なし）。
クラスタでは ID が最小のノードだけが作る。運用を始める前に:

1. 必要な権限だけを持つトークン（期限つき）を作る。
2. 管理者のトークンを新しく作ってから、最初のトークンを `DELETE /api/v1/tokens/{id}` で消す
   （最後の `tenant:admin` は消せないので、先に作る）。
3. `bootstrap-admin-token` のファイルを消すか、安全な場所へ移す（ノードはこのファイルを再び読まない）。

## 12.2 JWT

外部の認証基盤のトークンを使うときは、検証の鍵をローカルの JWKS ファイルで渡す。

```sh
senca-node ... --jwks /etc/senca/jwks.json \
  --jwt-issuer https://idp.example.com/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- **算法**: EdDSA（Ed25519）・ES256（P-256）・RS256（2048 bit 以上）。鍵の種類が算法を決め、ヘッダーの `alg` が
  鍵と違えば拒否。`none`・HS 系は使えない。
- **JWKS**: 各鍵に `kid` が要る。`use` があれば `sig`、`alg` があれば鍵の種類と一致すること。対応しない鍵が
  1 つでもあれば起動しない（黙って飛ばさない）。ファイルは起動時に 1 回だけ読む（鍵を替えたら再起動する）。
  トークンの中身で URL を取りに行くことはない。
- **ヘッダー**: `kid` が必須。`crit` があれば拒否。
- **クレーム**:
  - `exp` 必須。`nbf` は任意。時計のずれは 60 秒まで許す。
  - `iss` が `--jwt-issuer` と一致、`aud`（文字列か配列）に `--jwt-audience` を含むこと。
  - テナント: `--jwt-tenant-claim` の名前のクレーム（既定 `tenant`）。空・欠落は拒否。
  - 権限: `scope`（空白区切りの文字列か配列）、無ければ `scp`。知らない名前は無視する。
  - `sub` は監査ログの `subject` になる。

例（ペイロード）:

```json
{"iss": "https://idp.example.com/", "aud": "senca", "sub": "alice", "tenant": "acme",
 "scope": "stream:read stream:write", "exp": 1793669243}
```

Ed25519 の鍵でこの流れを確かめた: テナント `acme` の JWT で stream を作れ、`tenant:admin` の API は 403、
その stream は `default` のテナントからは見えなかった。

## 12.3 stream key（送り手の資格）

- RTMP・RTMPS・SRT・WHIP の送り手は stream key で認める。形は `sk_` + 32 バイトの乱数の base64url。
- サーバーには SHA-256 だけを残す。値が返るのは stream の作成と `POST /api/v1/streams/{id}/keys`（作り直し）の応答だけ。
  作り直すと古い鍵は使えなくなる。
- RTMP では URL の最後の部分（`rtmp://host:1935/live/<key>`）、SRT では streamid `publish:<key>`、
  WHIP では `Authorization: Bearer <key>`。
- stream key・トークンはログ・エラー・Debug 表示に出さない。
- 予約に `live` があれば、その時間帯の間だけ publish を受ける（[5. 録画](05-recording.md)）。

## 12.4 署名付きの再生 URL

- `POST /api/v1/streams/{id}/playback-url`（`subscribe`）でライブ、`POST /api/v1/recordings/{id}/playback-url`
  （`recording:read`）で録画（VOD）の URL を発行する。本文 `{"ttl_secs": N}`（省略時 300、1〜`--max-playback-ttl`）。
- 署名は HMAC-SHA256（`{kind}/{id}/{exp}`、kind は `live` か `vod`）の先頭 16 バイトの base64url。鍵は制御状態の
  再生用の鍵（32 バイトの乱数、初回に作る。クラスタでは全ノードで同じ）。
- ライブでは同じ署名が HLS（`/s/{exp}/{sig}/live/{id}/...`）・RTMP play（`/play/{id}?exp=&sig=`）・
  WHEP（`/whep/{id}?exp=&sig=`）・RTSP play に使える。録画は HLS（`/s/{exp}/{sig}/vod/{id}/...`）。
- 期限（`exp`、UNIX 秒）を過ぎたもの・署名の違うものは 403。
- **URL は持っている者なら誰でも期限まで使える**（視聴者ごとの束縛・取り消しは無い）。期限は短くし、
  必要な度に発行する。
- 再生用の鍵を替える API は無い。鍵は制御状態に入っているので、制御状態のファイルが漏れれば URL を偽造できる
  （[12.8](#128-データ置き場の保護)）。

## 12.5 秘密値

入力・出力の資格情報（RTMP の鍵、SRT のパスフレーズ、PaaS の資格情報）と字幕の API キーは、秘密値として
預けて参照で使う。URL に資格情報を書くこと（`rtmp://user:pass@...`）は拒否される。

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/secrets \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"youtube-key","value":"xxxx-xxxx"}'
# → {"id": "...", "name": "youtube-key", "ref": "secret:youtube-key", "created_at": "..."}
```

- 管理は `tenant:admin`（作成・一覧・削除）。値は二度と返らない。入力・出力・字幕が使っている秘密値は消せない（409）。
- 入力・出力は `credential_ref: "secret:{name}"`、字幕は `asr_credential_ref`・`translate_credential_ref` で参照する。
- 値は AES-256-GCM で暗号化し、暗号化した形だけを制御状態に保存します。関連データにテナントと ID を含めるため、
  暗号文を別のリソースへコピーしても復号できません。
- **単一ノード**: 鍵はノードの `data_dir/control/secret-key`（32 バイト、0600。無ければ初回起動時に作成）です。
- **クラスタ**: 鍵はクラスタ共通の**データ鍵**です。データ鍵はクラスタシークレット（`secret_file`）から HKDF-SHA256 で
  導出した鍵で AES-256-GCM により暗号化し、制御状態（`secret_data_key`）で全ノードへ複製します。まだ無い場合は各ノードが
  自分の `secret-key` を暗号化して提案し、最初に適用されたものが使われます（後から置き換わることはありません）。
  各ノードは制御状態からデータ鍵を復号するので、どのノードで登録した秘密値もすべてのノードで使えます。鍵ファイルを
  コピーする必要はありません。
  - クラスタシークレットは全ノードで同じにしてください。異なるノードはデータ鍵を復号できず、ログに
    `cannot open the cluster-wide secret key` と出力し、そのノードでは秘密値を使えません（登録は 503 になります）。
  - データ鍵がまだ届いていないノードで秘密値を登録すると 503（`retryable: true`）になります。
  - 制御状態のコピー（バックアップや Raft のログ）だけでは、クラスタシークレットを知らない限り秘密値を復号できません。
    クラスタシークレットには 32 バイトの乱数など推測できない値を使ってください（`openssl rand -hex 32`）。
  - **以前のバージョンからの移行**: 以前のバージョンのクラスタでは、秘密値は受け付けたノードの `secret-key` で暗号化して
    いました。このバージョンは復号時にデータ鍵の次にノードの `secret-key` も試し、起動時の初期化の後で、ノードの鍵でしか
    復号できない秘密値をデータ鍵で暗号化し直して置き換えます（ログに `re-sealed N secret(s)`）。古い値を暗号化したノードが
    一度このバージョンで起動すれば、すべてのノードで使えるようになります。
  - クラスタシークレットを変更するとデータ鍵を復号できなくなります（シークレットのローテーションには未対応です）。
- 単一ノードで `secret-key` を失うと、登録済みの秘密値は復元できません（登録し直してください）。クラスタでは、
  クラスタシークレットと制御状態があれば復元できます。

## 12.6 外へ出る接続の制限（SSRF 対策）

pull 入力・push 出力・HLS pull・字幕の送り先・PaaS の API は、利用者が指定した宛先へノードからつなぐ。
名前を解決した**後の IP** を検査し、既定で次を拒否する。

- ループバック（`127.0.0.0/8`・`::1`）、未指定（`0.0.0.0`・`::`）、`0.0.0.0/8`
- リンクローカル（`169.254.0.0/16`。クラウドのメタデータの `169.254.169.254` を含む、`fe80::/10`）
- 私設（`10.0.0.0/8`・`172.16.0.0/12`・`192.168.0.0/16`）、CGNAT（`100.64.0.0/10`）、ULA（`fc00::/7`）
- マルチキャスト、ブロードキャスト。IPv4 射影の IPv6（`::ffff:a.b.c.d`）は IPv4 として検査する

社内のカメラなどへつなぐときは、管理者が範囲を許す。

```sh
senca-node ... --egress-allow 10.1.0.0/16 --egress-allow 192.168.10.0/24
```

拒否されたときは、入力・出力の `status.last_error` とログに
`<IP> は許されていない宛先です（--egress-allow で許可が必要）` が出て、つなぎ直し（バックオフ）を続ける。

## 12.7 クラスタの mTLS

ノード間（Raft と中継）は、クラスタの設定に `tls_ca`・`tls_cert`・`tls_key` を書くと mTLS になる。
双方がクラスタの認証局で証明書を検証し、名前は `senca-cluster` で確かめる。

証明書の作り方（P-256）:

```sh
# 認証局（1 回だけ。鍵はノードに置かない）
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout cluster-ca.key -out cluster-ca.pem -days 3650 -subj "/CN=senca-cluster-ca"

# ノードごと（n1, n2, n3 ...）
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout n1.key -out n1.csr -subj "/CN=n1"
printf "subjectAltName=DNS:senca-cluster\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\n" > n1.ext
openssl x509 -req -in n1.csr -CA cluster-ca.pem -CAkey cluster-ca.key -CAcreateserial \
  -out n1.pem -days 825 -extfile n1.ext
```

- 証明書の SAN に `DNS:senca-cluster`、拡張鍵用途に `serverAuth` と `clientAuth` の両方が要る。
- 各ノードには `cluster-ca.pem`・自分の `nN.pem`・`nN.key` を置き、クラスタの設定で指す。
- 別の認証局の証明書のノードは拒否される。
- Raft と中継の接続では、TLS の中でさらにクラスタシークレット（`secret_file`）による相互のチャレンジレスポンス認証を行います（下記）。

上の手順で 2 台のクラスタが mTLS で組めることを確かめた（`tools/interop/cluster.sh` の `CLUSTER_TLS=1` と同じ形）。

### クラスタシークレットによるノードの認証

Raft と中継の接続では、mTLS の有無にかかわらず、接続の直後に相互のチャレンジレスポンス認証を行います:

1. 接続する側が、自分のノード番号と新しい 32 バイトの乱数を送ります。
2. 受け付ける側が、自分のノード番号、自分の新しい乱数、`HMAC-SHA256(K, "server" ‖ 用途 ‖ ノード番号 ‖ 双方の乱数)` を返します。
3. 接続する側は、相手が接続先のノードであることと MAC が正しいことを確認してから `HMAC-SHA256(K, "client" ‖ …)` を送り、
   受け付ける側がそれを検証します。

`K` はクラスタシークレットから HKDF-SHA256 で導出します（Raft 用と中継用で別の鍵）。シークレットも固定の値も
ネットワークに流れず、どの MAC も相手が選んだ乱数を含むため、記録したハンドシェイクを再送しても認証は通りません。
接続する側は相手を確認してから自分の証明を送ります。メッセージは固定長で、5 秒以内に終わらなければ接続を閉じます。

### 平文（合言葉だけ）の注意

`tls_*` を設定しない場合、ノード間は平文の TCP になり、相手の確認は上のチャレンジレスポンス認証だけになります。

- 制御状態の複製（トークン・stream key のハッシュ、**再生用の鍵**、暗号化した秘密値と秘密値の鍵など）と中継の
  メディアは暗号化されずに流れます。認証の後の通信の改ざんも防げません。
- 平文は閉じたネットワークでの試験だけに使う。

## 12.8 データ置き場の保護

- 単一ノードの `control/log`・`control/snapshot.json`、クラスタの `control/raft/` には、再生用の鍵（16 進の平文）、
  トークン・stream key のハッシュ、封じた秘密値が入る。再生用の鍵があれば、再生 URL と部屋の参加トークンを偽造できる。
- ノードは `secret-key`・`bootstrap-admin-token` を 0600 で作り、起動のたびに `control/` を 0700 にする（制御のファイルは他の利用者から読めない）。
  録画・セグメントは umask に従う。データ置き場は専用の利用者で動かし、
  `chmod 700` にする（Docker のイメージは専用の利用者 `senca` で、ボリュームの中に置く）。
- バックアップにも同じ秘密が入る。バックアップの置き場所も同じ扱いにする（[13. 運用](13-operations.md)）。

## 12.9 codec worker のサンドボックス

外部のコーデックライブラリ（GPU の SDK・dav1d・libvpx など）は `senca-node` の中に読み込まず、
`senca-codec-worker`（別プロセス）で動かす。worker は起動時に次を掛ける（Linux）。

| 段階 | 掛けるもの |
|---|---|
| ライブラリを読む前 | 標準出力の隔離（プロトコル用の fd を分ける）、`PR_SET_PDEATHSIG=SIGKILL`、`PR_SET_DUMPABLE=0`、`RLIMIT_CORE=0`、`RLIMIT_NOFILE=256`、`PR_SET_NO_NEW_PRIVS`、Landlock（使えるカーネルなら: ファイルの作成・書き込み・削除を `/dev` の下以外で禁止。読み取りは制限しない） |
| ライブラリを読んだ後 | seccomp-BPF の拒否リスト（全スレッド）: ネットワーク（socket・connect・bind・listen・accept）、プログラムの実行、ptrace・他プロセスのメモリ、mount、カーネルモジュール、kexec、bpf、鍵束、名前空間、reboot・swap・時計の設定など。`clone3` は `ENOSYS` |
| ジョブごと | `RLIMIT_CPU`（ジョブあたり 300 秒）。壁時計の時間切れはノードが見る（単発の仕事は 30 秒） |

- 環境変数は空にして起動し、`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH` だけを渡す。
- 掛けた結果は起動時にログに出る（例: `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`）。
  `GET /api/v1/devices`（`node:admin`）の `sandbox` でも見られる。
- worker が落ちる・固まる・おかしな出力を返すと、その仕事は失敗し、worker は作り直される（ノードは止まらない）。
- 既定ではメモリの上限（`RLIMIT_AS`）は掛けない。socket を拒否するので、GPU ドライバーの一部の補助機能
  （NVIDIA の persistenced への接続など）は使えない。GPU の実機での確認はまだ。

## 12.10 部屋の参加トークン

- `POST /api/v1/rooms/{id}/tokens`（`stream:write`）で発行する JWT 互換のトークン（EdDSA）。`iss` は `senca`、
  `aud` は `senca-rooms`、`kid` は `senca-rooms-1`。有効期間は `ttl_secs`（既定 600、10〜86400）。
  部屋・役割・`client_id`・`bundle_id`・metadata・許可を含む。無い部屋を指定すると部屋が自動で作られる。
- 署名鍵は再生用の鍵から導いた Ed25519 の鍵（`SHA-256("senca-rooms-join-token/1\0" || 再生用の鍵)`）。
  全ノードで同じなので、どのノードでも発行・検証できる。再生用の鍵が漏れれば参加トークンも偽造できる。
- ノードは認証 Webhook も匿名の参加も使わない（トークンが必須）。詳しくは [10. リアルタイムの部屋](10-rooms.md)。

## 12.11 HTTPS・RTMPS

`--https`・`--rtmps` と `--tls-cert`・`--tls-key`（PEM）で TLS 1.3 を受ける。鍵は ECDSA P-256・RSA・Ed25519。
TLS の実装は SENCA 自身のもので（暗号の演算は検証済みのライブラリ）、まだ独立したレビューを受けていない。
HLS・WHEP の配信量の多い経路を TLS にしたいときは、前段のリバースプロキシで終端してもよい（署名はパスと
クエリにあるので、プロキシを通しても検証できる。公開 URL は `--public-http-url` で合わせる）。

## 12.12 監査ログ

`data_dir/audit.log` に、状態を変える管理 API の操作を 1 行 1 JSON で追記する。

```json
{"action":"stream.create","request_id":"req_...","resource":"<資源 ID>","subject":"token:<トークン ID>","tenant":"default","ts":"2026-11-03T01:10:15.531Z"}
```

`subject` は API トークンなら `token:{ID}`、JWT なら `sub`。秘密・本文は書かない。
Idempotency-Key で覚えた応答からも `stream_key`・`token`・`secret` は消してから保存する（再送の応答では `null` と
`*_redacted: true`）。

## 12.13 まだ独立したレビューを受けていないもの

出荷 gate の G8（[docs/RELEASE.md](../../RELEASE.md)）は未了。次は担当した実装者と独立したレビューの報告書が無い。

- 暗号プロトコル: TLS 1.3（HTTPS・RTMPS・クラスタの mTLS・外向きの HTTPS）、DTLS 1.2・SRTP（WebRTC）、SRT の暗号、QUIC
- 認可: JWT、署名付き URL、API トークン
- 分散の所有権: fencing、mTLS

あわせて、24 時間の fuzz（今は 60 秒の smoke のみ）、72 時間の連続試験も未了
（[適合台帳](../../conformance/ledger.md)）。公開のネットワークに置くときは、この状態を前提に判断すること。
