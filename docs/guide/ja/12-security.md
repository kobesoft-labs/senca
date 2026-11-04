# 12. セキュリティ

この章では、SENCA のセキュリティに関わる機能と設定を説明します。
管理 API の認証と権限、利用者のアカウントとログイン（パスワード・MFA・OIDC）、配信者と視聴者の資格情報（stream key・署名付き URL・参加トークン）、
秘密値の保管、外部への接続の制限、クラスターの mTLS、codec worker のサンドボックスを扱います。

- 関連するページ: [3. 設定](03-configuration.md)、[9. クラスターとグリッド](09-cluster-grid.md)、[10. リアルタイムの部屋](10-rooms.md)、[13. 運用](13-operations.md)、[15. 管理 API と SDK](15-api-and-sdks.md)
- 設計の背景: [ADR-0002 依存関係と暗号の部品](../../dev/adr/0002-dependencies-and-crypto.md)、[ADR-0017 利用者のアカウント・ログインのセッション・OIDC](../../dev/adr/0017-users-sessions-oidc.md)、[ADR-0007 入力・出力・秘密値と外部への接続の制限](../../dev/adr/0007-inputs-outputs-egress.md)、[ADR-0009 分散の制御プレーン](../../dev/adr/0009-cluster.md)

## 12.1 管理 API の認証

管理 API（`/api/v1/...`）と SSE（`/api/v1/events`）は、`Authorization: Bearer <値>` ヘッダーで認証します。

- 値が `senca_` で始まれば **API トークン**、`sencas_` で始まれば利用者の**ログインのセッション**（[12.13](#1213-利用者のアカウントとログイン)）、
  それ以外は **JWT** として扱います（JWT は `--jwks` を指定したときだけ使えます）。
- 認証に失敗すると 401（`WWW-Authenticate: Bearer`）を返します。失敗の理由（未知のトークン、期限切れ、署名の誤りなど）は応答に含めず、
  ノードのログに `auth failed: <理由> request_id=...` として記録します。
- 認証なしで応答するのは、`/healthz`・`/readyz`、管理画面と配信用ページの静的ファイル、署名付きの再生 URL（`/s/...`）、
  ログインの操作（`GET /api/v1/auth/providers`・`POST /api/v1/auth/login`・`POST /api/v1/auth/login/mfa`・`POST /api/v1/auth/exchange`・
  `GET /api/v1/auth/oidc/{id}/start`・`GET /api/v1/auth/oidc/{id}/callback`）です。
  WHIP は stream key、WHEP は署名、部屋のシグナリングは参加トークンで認証します。

### API トークン

- 形式: `senca_` + 32 バイトの乱数を base64url で表したもの（43 文字）。サーバーには SHA-256 ハッシュだけを保存し、照合ではすべてのトークンを定数時間で比較します。
- トークンの値が返るのは、作成時の応答（`POST /api/v1/tokens`、201）だけです。一覧や取得では返りません。
- 作成時の本文: `{"name": "...", "scopes": ["..."], "expires_at": "RFC 3339"}`（`expires_at` は省略できます。過去の時刻は指定できません）。
- 作成には `tenant:admin` が必要です。**自分が持っている権限しか付与できません**。作成したトークンは、作成者と同じテナントに属します。
- 1 つのテナントで作成できるトークンは 100 個までです。テナントで最後の `tenant:admin` トークンは削除できません（409）。

### 権限（scope）

| 権限 | できること |
|---|---|
| `stream:read` | ストリーム・予約・入力・出力・パイプライン・部屋・メディアパイプラインの参照、静止画、SSE の購読 |
| `stream:write` | ストリームの作成・変更・削除・鍵の再発行、録画の開始・停止・変更（保持期間・リーガルホールド）、予約・入力・出力・パイプライン・部屋・メディアパイプラインの変更、部屋の参加トークンの発行 |
| `publish` | WHIP での配信（`POST /whip/{stream_id}` の `Authorization: Bearer` に、stream key の代わりに使えます。同じテナントの stream だけが対象です） |
| `subscribe` | ライブの再生 URL の発行 |
| `recording:read` | 録画の参照、録画の再生 URL・サムネイル・MP4 の書き出し |
| `recording:delete` | 録画の削除 |
| `tenant:admin` | `node:admin` 以外のすべて。加えて、トークンと秘密値の管理、監査ログの参照（`GET /audit`） |
| `node:admin` | テナントをまたぐ運用操作（`/nodes`・`/grid`・`/devices`、ノードの退避（drain）、デバイスの停止・再開、ノード・グリッドのイベント、監査ログの別のテナントの参照（`GET /audit` の `tenant`）） |

操作ごとに必要な権限は、[15. 管理 API と SDK](15-api-and-sdks.md) と OpenAPI（[docs/api/openapi.yaml](../../api/openapi.yaml)）を見てください。

### テナント

すべてのリソースはテナントに属し、他のテナントのリソースは見えません（404 になります）。最初の管理者トークンのテナントは `default` です。
API トークンは作成者と同じテナントにしか作れないため、**別のテナントを作るには JWT か OIDC を使います**（テナントは JWT のクレーム、
または OIDC の IdP の設定（[12.15](#1215-oidc-のシングルサインオン)）で決まります）。

### 最初の管理者トークン

初回の起動時に、ノードは `data_dir/control/bootstrap-admin-token`（パーミッション 0600）に管理者トークンを書き出します
（`tenant:admin`・`node:admin`、期限なし）。クラスターでは、ID が最も小さいノードだけが作成します。
運用を始める前に、次の手順で入れ替えてください。

1. 必要な権限だけを持つ、期限付きのトークンを作ります。
2. 新しい管理者トークンを作ってから、最初のトークンを `DELETE /api/v1/tokens/{id}` で削除します
   （最後の `tenant:admin` トークンは削除できないため、先に新しいものを作ります）。
3. `bootstrap-admin-token` ファイルを削除するか、安全な場所へ移します（ノードはこのファイルを再び読みません）。

### 要求の量の上限

総当たりや使いすぎを防ぐため、ノードは要求の数を数え、上限を超えると 429（`RATE_LIMITED`）を返します。応答の `Retry-After` には次に
送ってよいまでの秒数が、`details.limit` にはどの上限かが入ります。上限はノードごとに数えます（クラスター全体の合計ではありません）。

| `details.limit` | 数える単位 | 対象 | 既定（オプション） |
|---|---|---|---|
| `ip` | 送信元の IP アドレス | 管理 API（`/api/v1`）。認証の前に数えます | 1 秒に 300（`--rate-limit-ip`） |
| `token` | API トークン（JWT は subject） | 管理 API | 1 秒に 200（`--rate-limit-token`） |
| `auth_failures` | 送信元の IP アドレス | 認証の失敗（管理 API・SSE・WHIP の誤ったトークンや stream key） | 1 分に 30（`--rate-limit-auth-failures`） |
| `connect` | 送信元の IP アドレス | 接続の開始（`/signaling`・WHIP・WHEP・SSE） | 1 秒に 30（`--rate-limit-connect`） |
| `login` | 送信元の IP アドレス | ログインの失敗（パスワード・確認コード・OIDC） | 1 分に 10（`--rate-limit-login`） |
| `login_account` | アカウント（テナントと利用者名） | パスワード・確認コードの失敗 | 5 回まで。その後は 1 秒から倍々に待たせる（最長 15 分） |

- 1 秒あたりの上限は、短い間なら 2 倍まで受け付けます。
- 認証の失敗の回数を使い切った IP アドレスは、回復するまで、正しいトークンを送っても 429 になります。
- 送信元の IP アドレスは、ノードが受けた TCP 接続の相手です。リバースプロキシの背後に置く場合は、プロキシ側でも上限を設けてください。
- どの上限も `0` で無効にできます（[3. 設定](03-configuration.md)）。

## 12.2 JWT

外部の認証基盤（IdP）が発行したトークンを使うときは、署名の検証鍵をローカルの JWKS ファイルで指定します。

```sh
senca-node ... --jwks /etc/senca/jwks.json \
  --jwt-issuer https://idp.example.com/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- **アルゴリズム**: EdDSA（Ed25519）・ES256（P-256）・RS256（2048 ビット以上）。アルゴリズムは鍵の種類で決まり、ヘッダーの `alg` が
  鍵と合わなければ拒否します。`none` と HS 系は使えません。
- **JWKS**: 各鍵に `kid` が必要です。`use` があれば `sig`、`alg` があれば鍵の種類と一致している必要があります。対応していない鍵が
  1 つでもあると、ノードは起動しません（黙って読み飛ばすことはしません）。ファイルは起動時に 1 回だけ読みます（鍵を替えたら再起動してください）。
  トークンの内容をもとに URL へ鍵を取りに行くことはありません。
- **ヘッダー**: `kid` が必須です。`crit` があれば拒否します。
- **クレーム**:
  - `exp` は必須、`nbf` は任意です。時計のずれは 60 秒まで許容します。
  - `iss` が `--jwt-issuer` と一致し、`aud`（文字列または配列）に `--jwt-audience` が含まれている必要があります。
  - テナント: `--jwt-tenant-claim` で指定した名前のクレーム（既定は `tenant`）。空または欠落していれば拒否します。
  - 権限: `scope`（空白区切りの文字列または配列）、無ければ `scp`。未知の権限名は無視します。
  - `sub` は監査ログの `subject` になります。

ペイロードの例:

```json
{"iss": "https://idp.example.com/", "aud": "senca", "sub": "alice", "tenant": "acme",
 "scope": "stream:read stream:write", "exp": 1793669243}
```

この例のトークンでは、テナント `acme` にストリームを作成できます。`tenant:admin` が必要な API は 403 になり、
作成したストリームは `default` テナントからは見えません。

## 12.3 stream key（配信者の資格情報）

- RTMP・RTMPS・SRT・WHIP の配信者は stream key で認証します。形式は `sk_` + 32 バイトの乱数を base64url で表したものです。
- サーバーには SHA-256 ハッシュだけを保存します。値が返るのは、ストリームの作成時と `POST /api/v1/streams/{id}/keys`（再発行）の応答だけです。
  再発行すると古い鍵は使えなくなります。
- stream key の渡し方: RTMP では URL の最後の部分（`rtmp://host:1935/live/<key>`）、SRT では streamid `publish:<key>`、
  WHIP では `Authorization: Bearer <key>` です。
- WHIP では、stream key の代わりに `publish` の権限を持つ API トークン（または JWT）も使えます（同じテナントの stream だけ）。
  配信の機材に stream key を渡さずに期限付きのトークンを渡し、終わったら削除できます。
- stream key とトークンは、ログ・エラー・デバッグ表示に出力しません。
- 予約に `live` があるストリームは、その時間帯の間だけ配信を受け付けます（[5. 録画](05-recording.md)）。

## 12.4 署名付きの再生 URL

- `POST /api/v1/streams/{id}/playback-url`（`subscribe`）でライブの URL を、`POST /api/v1/recordings/{id}/playback-url`
  （`recording:read`）で録画（VOD）の URL を発行します。本文は `{"ttl_secs": N}` です（省略時は 300、範囲は 1〜`--max-playback-ttl`）。
- 署名は、HMAC-SHA256（対象は `{kind}/{id}/{exp}`。kind は `live` か `vod`）の先頭 16 バイトを base64url で表したものです。
  鍵は制御状態に保存された再生用の鍵（32 バイトの乱数。初回に作成し、クラスターでは全ノードで共通）です。
- ライブでは、同じ署名を HLS（`/s/{exp}/{sig}/live/{id}/...`）・RTMP play（`/play/{id}?exp=&sig=`）・
  WHEP（`/whep/{id}?exp=&sig=`）・RTSP play で使えます。録画は HLS（`/s/{exp}/{sig}/vod/{id}/...`）です。
- 期限（`exp`、UNIX 秒）を過ぎた URL や、署名が一致しない URL は 403 になります。
- **URL を知っている人は誰でも、期限まで使えます**（視聴者ごとの紐付けや取り消しの機能はありません）。期限は短くし、
  必要になるたびに発行してください。
- 再生用の鍵を入れ替える API はありません。鍵は制御状態に保存されているため、制御状態のファイルが漏えいすると URL を偽造できます
  （[12.8](#128-データディレクトリの保護)）。

## 12.5 秘密値

入力・出力の資格情報（RTMP の鍵、SRT のパスフレーズ、PaaS の資格情報）と字幕の API キーは、秘密値として登録し、参照名で使います。
URL に資格情報を書くこと（`rtmp://user:pass@...`）はできません。

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/secrets \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"youtube-key","value":"xxxx-xxxx"}'
# → {"id": "...", "name": "youtube-key", "ref": "secret:youtube-key", "created_at": "..."}
```

- 秘密値の管理（作成・一覧・削除）には `tenant:admin` が必要です。登録した値は二度と返りません。入力・出力・字幕が使用中の秘密値は削除できません（409）。
- 入力・出力では `credential_ref: "secret:{name}"`、字幕では `asr_credential_ref`・`translate_credential_ref` で参照します。
- 値は AES-256-GCM で暗号化し、暗号化した形だけを制御状態に保存します。関連データにテナントと ID を含めるため、
  暗号文を別のリソースへコピーしても復号できません。
- **単一ノード**: 鍵はノードの `data_dir/control/secret-key`（32 バイト、0600。無ければ初回起動時に作成）です。
- **クラスター**: 鍵はクラスター共通の**データ鍵**です。データ鍵は、クラスターシークレット（`secret_file`）から HKDF-SHA256 で
  導出した鍵を使って AES-256-GCM で暗号化し、制御状態（`secret_data_key`）を通じて全ノードへ複製します。データ鍵がまだ無い場合は、
  各ノードが自分の `secret-key` を暗号化して提案し、最初に適用されたものが使われます（後から置き換わることはありません）。
  各ノードは制御状態からデータ鍵を復号するので、どのノードで登録した秘密値もすべてのノードで使えます。鍵ファイルを
  コピーする必要はありません。
  - クラスターシークレットは全ノードで同じにしてください。異なるノードはデータ鍵を復号できず、ログに
    `cannot open the cluster-wide secret key` と出力します。そのノードでは秘密値を使えません（登録は 503 になります）。
  - データ鍵がまだ届いていないノードで秘密値を登録すると、503（`retryable: true`）になります。
  - 制御状態のコピー（バックアップや Raft のログ）だけでは、クラスターシークレットを知らない限り秘密値を復号できません。
    クラスターシークレットには、32 バイトの乱数など推測できない値を使ってください（`openssl rand -hex 32`）。
  - **以前のバージョンからの移行**: 以前のバージョンのクラスターでは、秘密値は登録を受け付けたノードの `secret-key` で暗号化して
    いました。このバージョンは、復号時にデータ鍵の次にノードの `secret-key` も試します。また起動時の初期化の後で、ノードの鍵でしか
    復号できない秘密値をデータ鍵で暗号化し直します（ログに `re-sealed N secret(s)`）。古い値を暗号化したノードが
    一度このバージョンで起動すれば、その秘密値はすべてのノードで使えるようになります。
  - クラスターシークレットを変更すると、データ鍵を復号できなくなります（クラスターシークレットのローテーションには対応していません）。
- 単一ノードで `secret-key` を失うと、登録済みの秘密値は復元できません（登録し直してください）。クラスターでは、
  クラスターシークレットと制御状態があれば復元できます。

## 12.6 外部への接続の制限（SSRF 対策）

pull 入力・push 出力・HLS の pull・字幕の送信先・PaaS の API・部屋の Webhook では、利用者が指定した宛先へノードから接続します。
ノードは名前解決**後の IP アドレス**を検査し、既定では次の宛先を拒否します。

- ループバック（`127.0.0.0/8`・`::1`）、未指定アドレス（`0.0.0.0`・`::`）、`0.0.0.0/8`
- リンクローカル（`169.254.0.0/16`。クラウドのメタデータの `169.254.169.254` を含む。`fe80::/10`）
- プライベートアドレス（`10.0.0.0/8`・`172.16.0.0/12`・`192.168.0.0/16`）、CGNAT（`100.64.0.0/10`）、ULA（`fc00::/7`）
- マルチキャスト、ブロードキャスト。IPv4 射影 IPv6 アドレス（`::ffff:a.b.c.d`）は IPv4 として検査します

社内のカメラなどへ接続するときは、管理者が範囲を許可します。

```sh
senca-node ... --egress-allow 10.1.0.0/16 --egress-allow 192.168.10.0/24
```

拒否されると、入力・出力の `status.last_error` とログに
`<宛先> は許されていない宛先です（--egress-allow で許可が必要）` と表示され、再接続（バックオフ付き）を続けます。

## 12.7 クラスターの mTLS

クラスターの設定に `tls_ca`・`tls_cert`・`tls_key` を書くと、ノード間の通信（Raft と中継）が mTLS になります。
双方がクラスターの認証局で相手の証明書を検証し、名前 `senca-cluster` を確認します。

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

- 証明書の SAN に `DNS:senca-cluster`、拡張鍵用途に `serverAuth` と `clientAuth` の両方が必要です。
- 各ノードに `cluster-ca.pem`・自分の `nN.pem`・`nN.key` を置き、クラスターの設定でそれらを指定します。
- 別の認証局が発行した証明書を持つノードは拒否されます。
- Raft と中継の接続では、TLS の内側でさらに、クラスターシークレット（`secret_file`）による相互のチャレンジレスポンス認証を行います（次項）。

### クラスターシークレットによるノードの認証

Raft と中継の接続では、mTLS の有無にかかわらず、接続の直後に相互のチャレンジレスポンス認証を行います。

1. 接続する側が、自分のノード番号と新しい 32 バイトの乱数を送ります。
2. 受け付ける側が、自分のノード番号、自分の新しい乱数、`HMAC-SHA256(K, "server" ‖ 用途 ‖ ノード番号 ‖ 双方の乱数)` を返します。
3. 接続する側は、相手が接続先のノードであることと MAC が正しいことを確認してから `HMAC-SHA256(K, "client" ‖ …)` を送り、
   受け付ける側がそれを検証します。

`K` はクラスターシークレットから HKDF-SHA256 で導出します（Raft 用と中継用で別の鍵です）。シークレットも固定値も
ネットワークに流れず、どの MAC も相手が選んだ乱数を含むため、記録したハンドシェイクを再送しても認証は通りません。
接続する側は相手を確認してから自分の証明を送ります。メッセージは固定長で、5 秒以内に終わらなければ接続を閉じます。

### 平文（クラスターシークレットだけ）で使う場合

`tls_*` を設定しない場合、ノード間は平文の TCP になり、相手の確認は上のチャレンジレスポンス認証だけになります。

- 制御状態の複製（トークン・stream key のハッシュ、**再生用の鍵**、暗号化した秘密値と秘密値の鍵など）と中継の
  メディアは、暗号化されずに流れます。認証の後の通信の改ざんも防げません。
- 平文での運用は、外部から隔離された信頼できるネットワークに限ってください。

## 12.8 データディレクトリの保護

- 単一ノードの `control/log`・`control/snapshot.json`、クラスターの `control/raft/` には、再生用の鍵（16 進の平文）、
  トークンと stream key のハッシュ、暗号化した秘密値が入っています。再生用の鍵があれば、再生 URL と部屋の参加トークンを偽造できます。
  利用者のパスワードのハッシュ（Argon2id）、ログインのセッションと回復コードのハッシュ、暗号化した認証アプリの秘密も入っています。
- ノードは `secret-key`・`bootstrap-admin-token` をパーミッション 0600 で作成し、起動のたびに `control/` を 0700 にします（制御用のファイルは他のユーザーから読めません）。
  録画とセグメントは umask に従います。データディレクトリは専用のユーザーで運用し、
  `chmod 700` にしてください（Docker イメージは専用ユーザー `senca` で動き、データはボリュームに置きます）。
- バックアップにも同じ秘密情報が含まれます。バックアップの保存先も同じように保護してください（[13. 運用](13-operations.md)）。

## 12.9 codec worker のサンドボックス

外部のコーデックライブラリ（GPU の SDK・dav1d・libvpx など）は `senca-node` のプロセスには読み込まず、
別プロセスの `senca-codec-worker` で動かします。worker は起動時に次の制限をかけます（Linux）。

| 段階 | かける制限 |
|---|---|
| ライブラリを読み込む前 | 標準出力の隔離（プロトコル用の fd を分ける）、`PR_SET_PDEATHSIG=SIGKILL`、`PR_SET_DUMPABLE=0`、`RLIMIT_CORE=0`、`RLIMIT_NOFILE=256`、`PR_SET_NO_NEW_PRIVS`、Landlock（対応するカーネルの場合: ファイルの作成・書き込み・削除を `/dev` の下以外で禁止。読み取りは制限しない） |
| ライブラリを読み込んだ後 | seccomp-BPF の拒否リスト（全スレッド）: ネットワーク（socket・connect・bind・listen・accept）、プログラムの実行、ptrace・他プロセスのメモリ、mount、カーネルモジュール、kexec、bpf、キーリング、名前空間、reboot・swap・時計の設定など。`clone3` は `ENOSYS` |
| ジョブごと | `RLIMIT_CPU`（1 ジョブあたり 300 秒）。経過時間のタイムアウトはノードが監視します（単発のジョブは 30 秒） |

- 環境変数を空にして起動し、`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH` だけを渡します。
- 適用した制限は起動時にログに出力されます（例: `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`）。
  `GET /api/v1/devices`（`node:admin`）の `sandbox` でも確認できます。
- worker が異常終了したり、応答しなくなったり、不正な出力を返したりすると、そのジョブは失敗し、worker は起動し直されます（ノードは止まりません）。
- 既定ではメモリの上限（`RLIMIT_AS`）はかけません。socket を拒否するため、GPU ドライバーの一部の補助機能
  （NVIDIA の persistenced への接続など）は使えません。

## 12.10 部屋の参加トークン

- `POST /api/v1/rooms/{id}/tokens`（`stream:write`）で発行する、JWT 互換のトークン（EdDSA）です。`iss` は `senca`、
  `aud` は `senca-rooms`、`kid` は `senca-rooms-1` です。有効期間は `ttl_secs`（既定 600、範囲 10〜86400）で指定します。
  トークンには、部屋・役割・`client_id`・`bundle_id`・metadata・許可が含まれます。存在しない部屋を指定すると、部屋が自動で作成されます。
- 署名鍵は、再生用の鍵から導出した Ed25519 の鍵（`SHA-256("senca-rooms-join-token/1\0" || 再生用の鍵)`）です。
  全ノードで共通なので、どのノードでも発行・検証できます。再生用の鍵が漏えいすると、参加トークンも偽造できます。
- 既定では、部屋への参加に参加トークンが必須です。認証 Webhook（`auth_webhook_url`・`--rooms-auth-webhook`）を設定すると、
  トークンの検証後に Webhook にも問い合わせます。`--rooms-auth-mode only` のときは、トークンの代わりに Webhook が参加を認めます。
  詳しくは [10. リアルタイムの部屋](10-rooms.md) を見てください。

## 12.11 HTTPS・RTMPS

`--https`・`--rtmps` と `--tls-cert`・`--tls-key`（PEM）を指定すると、TLS 1.3 で接続を受け付けます。鍵は ECDSA P-256・RSA・Ed25519 に対応しています。
TLS のプロトコル処理は SENCA 自身の実装で、暗号の演算には検証済みのライブラリを使っています。
HLS や WHEP など配信量の多い経路を TLS にしたいときは、前段のリバースプロキシで TLS を終端してもかまいません（署名はパスと
クエリに含まれるので、プロキシを通しても検証できます。公開 URL は `--public-http-url` で合わせてください）。

## 12.12 監査ログ

`data_dir/audit.log` に、状態を変更する管理 API の操作を 1 行 1 JSON で追記します。

```json
{"action":"stream.create","request_id":"req_...","resource":"<リソース ID>","subject":"token:<トークン ID>","tenant":"default","ts":"2026-11-03T01:10:15.531Z"}
```

`subject` は、API トークンなら `token:{ID}`、ログインのセッションなら `user:{利用者 ID}`、JWT なら `sub` の値です。秘密情報やリクエストの本文は記録しません。
Idempotency-Key のために保存する応答からも、`stream_key`・`token`・`secret`・`temporary_password`・`recovery_codes`・`otpauth_uri`・
`enrollment_token`・`mfa_token` を消してから保存します（再送時の応答では、これらは `null` になり、`*_redacted: true` が付きます）。

ログインと利用者の操作は、次の `action` で記録します（パスワード・確認コード・回復コード・セッションの値は記録しません）。

| `action` | 内容 |
|---|---|
| `auth.login` | ログイン。`outcome`（`success`・`failure`・`mfa_required`・`callback`）、`method`（`password`・`password+totp`・`password+recovery_code`・`oidc:{id}`）、`reason`、`ip` を付けます。知らない利用者名での失敗は `subject: anonymous` で、試された利用者名は記録しません |
| `auth.logout`・`auth.session.revoke` | ログアウト・自分のセッションの失効 |
| `auth.password.change` | 自分のパスワードの変更 |
| `auth.mfa.enable`・`auth.mfa.disable`・`auth.mfa.recovery_codes`・`auth.mfa.recovery_code_used` | MFA の登録・解除・回復コードの作り直し・回復コードの使用 |
| `user.create`・`user.update`・`user.delete` | 利用者の作成・変更（役割・権限が変わったか、無効化）・削除。OIDC で自動で作った・役割を決め直したときは `by: "oidc:{id}"` |
| `user.password.reset`・`user.mfa.reset`・`user.sessions.revoke`・`user.oidc.link` | 管理者によるパスワードの再設定・MFA の解除・セッションの失効、IdP の利用者との結び付け |

### 回転

`audit.log` が `--audit-log-max-bytes`（既定 64 MiB）以上になると、`audit.log.{N}` に名前を変えて新しい `audit.log` に書き始めます。
`N` は 1 から順に増える番号で、大きいほど新しいファイルです。回転したファイルは新しいものから `--audit-log-keep`（既定 8）個を残し、
それより古いものは削除します（既定では、ノードあたりおよそ 576 MiB まで）。`--audit-log-max-bytes 0` にすると回転しません。
1 行は 1 回の書き込みで追記するので、途中でノードが止まっても、それまでの行は壊れません（書きかけの行は読み出しのときに飛ばします）。
別の場所へ保管したい場合は、回転したファイル（`audit.log.{N}`）を集めてください。

### API で読む

`GET /api/v1/audit` で、記録を時刻の新しい順に読めます（`tenant:admin` が必要です）。

```sh
curl -s -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:8080/api/v1/audit?since=2026-11-03T00:00:00Z&action=stream.&limit=50"
```

```json
{"items":[{"action":"stream.create","node":"3d6d4c49...","node_name":"node-1","request_id":"req_...","resource":"<リソース ID>",
  "subject":"token:<トークン ID>","tenant":"default","ts":"2026-11-03T01:10:15.531Z"}],"next_cursor":"eyJ0Ijox...","unavailable_nodes":[]}
```

| クエリ | 意味 |
|---|---|
| `since`・`until` | 時刻の範囲（RFC 3339。`since` は含み、`until` は含みません） |
| `action` | 操作の種類。完全に一致するもの（`stream.create`）。`.` か `*` で終われば前方一致（`stream.`） |
| `subject`・`resource` | 操作した主体・対象の資源に完全に一致するもの |
| `q` | `action`・`subject`・`resource`・`request_id` のいずれかに含まれる文字列（大文字・小文字を区別しません） |
| `node` | そのノード（ID または名前）の記録だけ |
| `tenant` | 読むテナント。既定は自分のテナントで、別のテナントと `*`（すべて）は `node:admin` も必要です |
| `limit`・`cursor` | ほかの一覧と同じページング（`next_cursor` が `null` になるまでたどります） |

- 見えるのは自分のテナントの記録だけです。
- ファイルは後ろから少しずつ読むので、記録が大きくてもファイル全体をメモリに読み込みません。1 回の要求で読む量には上限（ノードごとに 16 MiB）があり、
  上限に達すると、項目が `limit` より少なくても `next_cursor` を返します。回転したファイルもまたいで読みます（削除したファイルの記録は返しません）。
- JSON として読めない行は飛ばします。記録には今後フィールドが増えることがあります。

**クラスター**: 記録は要求を受けたノードの `audit.log` にあります。`GET /audit` を受けたノードは、各ノードにノード間の接続
（クラスターの共有シークレットによる相互認証。設定していれば mTLS。[12.7](#127-クラスターの-mtls)）で問い合わせ、時刻の新しい順に合わせて返します。
各項目の `node`・`node_name` は記録したノードです。呼び出し元のトークンはほかのノードへ送りません。テナントの絞り込みは、要求を受けたノードが
呼び出し元の権限から決めて問い合わせに含めます。5 秒以内に答えなかったノードは `unavailable_nodes` に入り、そのノードの記録は含まれません
（`node` でそのノードを指定した場合は 503）。ノードの時計がずれていると、ノードをまたいだ並び順もずれます（時刻は NTP などでそろえてください）。

## 12.13 利用者のアカウントとログイン

管理画面や管理 API を人が使うときは、API トークンを配る代わりに**利用者のアカウント**を作り、ログインしてもらいます。
ログインの方法は 2 つあり、両方を同時に使えます。

- **パスワード**（と、任意で認証アプリの確認コード。[12.14](#1214-多要素認証mfa)）
- **OIDC のシングルサインオン**（Google Workspace・Microsoft Entra ID・Keycloak・Auth0 など。[12.15](#1215-oidc-のシングルサインオン)）

ログインすると**セッション**（`sencas_` で始まる値）が返ります。API トークンと同じく `Authorization: Bearer` に付けて使います。

### 利用者

- 利用者はテナントごとに作ります。他のテナントの利用者は見えません（404）。1 つのテナントに 1000 人まで作れます。
- 利用者名は 1〜64 文字の英数字と `. _ @ + -` です。大文字と小文字は区別しません（`Alice` と `alice` は同じ利用者名です）。メールアドレスもテナントの中で重複できません。
- パスワードは省略できます。パスワードの無い利用者は、OIDC でだけログインします。
- 利用者の権限は**役割**と個別の権限の和です。

| 役割 | 権限 |
|---|---|
| `admin` | `tenant:admin`（`node:admin` 以外のすべて） |
| `operator` | `stream:read`・`stream:write`・`publish`・`subscribe`・`recording:read` |
| `viewer` | `stream:read`・`recording:read`・`subscribe` |

  役割にない権限（`recording:delete`・`node:admin` など）は `scopes` で個別に渡します。役割や権限を変えると、すぐに効きます。

### 最初の管理者を作る

`GET /api/v1/auth/providers`（認証は不要です）の `setup_required` が `true` なら、利用者はまだ 1 人もいません。
管理画面はこのとき「最初の管理者を作る」画面を出します。API では、最初の管理者トークン（[12.1](#121-管理-api-の認証)）で利用者を作ります。

```sh
TOKEN=$(cat /var/lib/senca/control/bootstrap-admin-token)
curl -s -X POST http://127.0.0.1:8080/api/v1/users \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"username": "alice", "email": "alice@example.com", "password": "長くて推測されにくいパスワード", "roles": ["admin"]}'
```

利用者ができた後も、最初の管理者トークンはそのまま使えます。入れ替えの手順は [12.1](#121-管理-api-の認証) を見てください。

### パスワード

- 12〜256 文字です（文字数で数えます。日本語も使えます）。利用者名・メールアドレスと同じものは使えません。文字列は正規化せずに、そのまま扱います。
- サーバーには Argon2id（19 MiB・2 回・並列 1、利用者ごとの塩）のハッシュだけを保存します。パラメーターはハッシュの中に記録しているので、
  将来パラメーターを上げたときは、次のログインで新しいパラメーターのハッシュに置き換えます。
- 本人は `POST /api/v1/auth/password`（今のパスワードが必要）で変えられます。変えると、そのとき使っていたセッション以外は失効します。
- 管理者は `POST /api/v1/users/{id}/password` で再設定できます。パスワードを渡さなければ、ノードが 20 文字の仮のパスワードを作って、その応答でだけ返します。
  仮のパスワードでログインした利用者は、パスワードを変えるまで他の操作ができません（`pending: ["password_change"]`）。

### パスワードでのログイン

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/auth/login -H 'Content-Type: application/json' \
  -d '{"username": "alice", "password": "長くて推測されにくいパスワード"}'
# → {"status": "ok", "token": "sencas_...", "session": {...}, "user": {...}, "pending": [], ...}
```

- `default` 以外のテナントの利用者は、本文に `"tenant": "acme"` を加えます。
- 認証アプリを登録した利用者には `{"status": "mfa_required", "mfa_token": "..."}` が返ります（[12.14](#1214-多要素認証mfa)）。
- 利用者がいない・パスワードが違う・利用者が無効のどれでも、同じ 401 を返します。利用者がいない場合も同じだけ計算するので、
  応答の内容や時間から利用者の有無は分かりません。
- **失敗の上限**: 送信元の IP アドレスごとに 1 分に 10 回（`--rate-limit-login`）、アカウントごとに 5 回まで失敗できます。アカウントは 6 回目の失敗から
  1 秒、2 秒、4 秒…と待たせ（最長 15 分。1 時間失敗がなければ数え直します）、その間は正しいパスワードでも 429（`details.limit` は `login_account`）です。
  IP アドレスの上限を超えたときは `details.limit` が `login` です。どちらもノードごとに数えます。
- `--auth-password-login off` にすると、パスワードではログインできなくなります（OIDC だけにする場合）。

### セッション

- セッションは、最後に使ってから 8 時間（`--auth-session-idle`）使わなければ失効します。使い続けていても、ログインから 7 日（`--auth-session-lifetime`）で失効します。
- 次のときにも失効します: ログアウト（`POST /api/v1/auth/logout`）、パスワードの変更・再設定、利用者の無効化・削除、役割・権限の変更。
- 自分のセッションは `GET /api/v1/auth/sessions` で一覧でき（ログインした IP アドレスとブラウザーの情報付き）、`DELETE /api/v1/auth/sessions/{id}` で失効させられます。
  管理者は `GET`・`DELETE /api/v1/users/{id}/sessions` で利用者のセッションを扱えます。1 人あたり 20 まで持て、超えると古いものから消えます。
- サーバーにはセッションの値の SHA-256 だけを保存します。クラスターでは全ノードに複製されるので、どのノードでログインしても、どのノードでも使えます。
- 最後に使った時刻は、要求のたびではなく数分おき（既定では 5 分おき）に記録します（クラスターの合意ログへの書き込みを減らすためです）。
  そのため、待ち時間による失効は、最大でその分だけ早くなることがあります。
- 監査ログの `subject` は `user:{利用者 ID}` です。`GET /api/v1/auth/me` で、自分の利用者・テナント・権限・セッションの期限を確かめられます。

### 利用者の管理

利用者の管理には `tenant:admin` が必要です（`/api/v1/users`）。

| 操作 | API |
|---|---|
| 作成・一覧・取得 | `POST /users`、`GET /users`、`GET /users/{id}` |
| 役割・権限・無効化・表示名・メールアドレスの変更 | `PATCH /users/{id}`（`If-Match` が必要） |
| 削除 | `DELETE /users/{id}` |
| パスワードの再設定 | `POST /users/{id}/password` |
| MFA を外す（認証アプリをなくしたとき） | `DELETE /users/{id}/mfa` |
| セッションの一覧・すべて失効 | `GET /users/{id}/sessions`、`DELETE /users/{id}/sessions` |

- **自分が持っている権限しか渡せません**（403）。`node:admin` は `node:admin` を持つ主体だけが渡せます。
  また、自分が持っていない権限を持つ利用者（例: `node:admin` を持つ利用者）は、変更・削除・パスワードの再設定ができません（403）。
- **テナントの最後の管理者**（有効で `tenant:admin` を持つ利用者）は、削除・無効化・降格できません（409）。先に別の管理者を作ってください。
- 利用者の作成・変更・削除は、SSE（`GET /api/v1/events`）に `user.created`・`user.updated`・`user.deleted` として流れます（`tenant:admin` を持つ主体にだけ）。

### 管理画面での操作

管理画面（`/console/`）のログインの画面は、ノードの設定に合わせて変わります。

- **利用者がまだいないとき**: 「SENCA へようこそ」の画面になります。最初の管理者トークンを貼り付けてから、管理者のユーザー名とパスワードを決めると、
  そのままログインします。
- **ふだん**: IdP ごとの「○○ でログイン」のボタンと、ユーザー名・パスワードの欄が出ます。認証アプリを登録した利用者には、続けて 6 桁のコードを聞きます
  （「回復コードを使う」で回復コードに切り替えられます）。失敗が続いて待たされているときは、次に試せるまでの時間を数えて出します。
- **API トークン・JWT**: 画面の下の「API トークンでログイン」から、今までどおり貼り付けてログインできます。
- 開こうとしていたページ（`#/streams/...` など）は、ログインの後にそのまま開きます。OIDC でも戻ってきます。
- 仮のパスワードでログインしたときや、`--auth-require-mfa` で認証アプリが未登録のときは、パスワードの変更・認証アプリの登録の画面が先に出ます。
  済むまでほかの画面は使えません（右上の「ログアウト」で抜けられます）。
- 使っている途中でセッションの期限が切れたときは、その画面のまま「もう一度ログインしてください」のダイアログが出ます。入り直すと、続きから使えます。

上の帯の右端（アカウントのメニュー）から**アカウント**の画面を開くと、パスワードの変更、認証アプリの登録（QR コードはブラウザーの中で作ります）・
回復コードの作り直し・解除、ログイン中の端末の一覧とログアウトができます。

管理者は、左のメニューの**ユーザー**で利用者を管理します。追加のときに役割（管理者・オペレーター・閲覧者）を説明付きで選び、
「仮のパスワードを作る」を選ぶと、そのパスワードを 1 度だけ表示します（本人は最初のログインで新しいパスワードを決めます）。
役割の変更・無効化・パスワードの再設定・認証アプリの解除・ログイン中の端末の確認と失効・削除は、各行の「…」から行います。
最後の管理者を降格・無効化・削除しようとすると、理由と次にすることを説明します。

## 12.14 多要素認証（MFA）

パスワードでログインする利用者は、認証アプリ（TOTP。Google Authenticator・Microsoft Authenticator・1Password など）の確認コードを
2 つ目の要素として登録できます。

1. `POST /api/v1/auth/mfa/totp/setup`（本文に今のパスワード `{"password": "..."}`）で、`otpauth_uri`（QR コード用）と `secret`（手で入力する場合の文字列）、
   `enrollment_token` を受け取ります。秘密はこのときにだけ表示されます。
2. 認証アプリに登録し、表示された 6 桁のコードを `POST /api/v1/auth/mfa/totp/confirm`（`{"enrollment_token": "...", "code": "123456"}`）に送ると有効になります。
3. 応答の**回復コード 10 個**（`xxxx-xxxx-xxxx-xxxx`）を安全な場所に保管してもらいます。回復コードもこのときにだけ表示されます。

有効にした後のログイン:

```sh
curl -s -X POST .../api/v1/auth/login -d '{"username": "alice", "password": "..."}'      # → {"status": "mfa_required", "mfa_token": "..."}
curl -s -X POST .../api/v1/auth/login/mfa -d '{"mfa_token": "...", "code": "123456"}'   # → {"status": "ok", "token": "sencas_..."}
```

- 認証アプリの代わりに、回復コード（`{"mfa_token": "...", "recovery_code": "abcd-efgh-jkmn-pqrs"}`）も使えます。回復コードは 1 回ずつしか使えません。
- 同じ確認コードは 2 度使えません（30 秒ごとに変わるコードのうち、使ったものより後のものだけを受け付けます）。時計のずれは前後 30 秒まで許します。
- 回復コードの作り直し（`POST /api/v1/auth/mfa/recovery-codes`）と MFA の解除（`POST /api/v1/auth/mfa/disable`）には、今のパスワードか確認コード
  （`{"password": "..."}` または `{"code": "..."}`）が必要です。
- `--auth-require-mfa` を付けたノードでは、パスワードでログインする利用者は MFA が必須です。未登録の利用者は、ログインすると
  `pending: ["mfa_enrollment"]` のセッションを受け取り、登録を終えるまで他の操作ができません。このノードでは MFA を解除できません。
- 認証アプリの秘密は、秘密値と同じ鍵（[12.5](#125-秘密値)）で暗号化して保存します。回復コードはハッシュだけを保存します。
- OIDC でログインする利用者には、SENCA の MFA は使いません。IdP の側で MFA を設定してください。

## 12.15 OIDC のシングルサインオン

ノードが OpenID Connect の relying party になり、IdP のログイン画面で認証します（認可コードフロー。PKCE（S256）・`state`・`nonce` を必ず使います）。

### 流れ

1. ログイン画面のボタンが、`GET /api/v1/auth/providers` の `oidc[].start_url`（`/api/v1/auth/oidc/{id}/start?return_to=/console/...`）をブラウザーで開きます。
2. ノードは IdP へリダイレクトします。ログインの途中の値は、暗号化したクッキー（`senca_oidc`、HttpOnly、10 分）に入れます。
3. IdP から `/api/v1/auth/oidc/{id}/callback` に戻ると、ノードは ID トークンを受け取って検証し、利用者を決めて、
   管理画面の `/console/#/auth/complete?code=...`（失敗したときは `?error=...`）へリダイレクトします。
4. 管理画面は、同じブラウザーから `POST /api/v1/auth/exchange`（`{"code": "..."}`）を送ってセッションを受け取ります。コードは 60 秒・1 回だけ有効です。

クラスターでは、手順 1・3・4 がそれぞれ別のノードに届いてもかまいません（クッキーはクラスター共通の鍵で暗号化しています）。

### IdP の側の設定

- **リダイレクト URI** に `{公開 URL}/api/v1/auth/oidc/{id}/callback` を登録します（`{公開 URL}` は `--public-http-url`）。
  リバースプロキシの背後などで違う URL にする場合は、`redirect_url` で指定します。`--public-http-url` が `https://` なら、クッキーに `Secure` を付けます。
- クライアントの種類は、クライアントシークレットを使うもの（confidential）を勧めます。シークレットはファイルに書いて `client_secret_file` で指定します。
  シークレットの無いクライアント（public client）も使えます。
- スコープは既定で `openid email profile` です。

### SENCA の側の設定

IdP が 1 つなら起動オプションで、複数なら JSON ファイル（`--oidc-config`）で指定します。

```sh
senca-node ... --public-http-url https://senca.example.com \
  --oidc-id keycloak --oidc-display-name "社内アカウント" \
  --oidc-issuer https://sso.example.com/realms/media --oidc-client-id senca \
  --oidc-client-secret-file /etc/senca/keycloak-client-secret \
  --oidc-role-map groups=senca-admins:admin --oidc-default-role viewer --oidc-auto-provision
```

`--oidc-config` のファイルは `{"providers": [ ... ]}` の形で、各要素は次の項目を持ちます。

| 項目 | 既定 | 説明 |
|---|---|---|
| `id` | （必須） | URL に入る名前（英小文字・数字・`-`・`_`、1〜32 文字） |
| `display_name` | `id` | ログインのボタンに出す名前 |
| `issuer` | （必須） | 発行者の URL。`{issuer}/.well-known/openid-configuration` を取得します |
| `client_id` | （必須） | クライアント ID |
| `client_secret_file` | なし | クライアントシークレットのファイル（前後の空白は除きます）。無ければ public client |
| `scopes` | `["openid", "email", "profile"]` | 要求するスコープ（`openid` は必須） |
| `redirect_url` | `{--public-http-url}/api/v1/auth/oidc/{id}/callback` | IdP に登録したリダイレクト URI |
| `allowed_email_domains` | `[]`（制限なし） | ログインを許すメールアドレスのドメイン |
| `require_email_verified` | `true` | メールアドレスを使う（ドメインの制限・既存の利用者との結び付け）とき、`email_verified: true` を求める |
| `tenant` | `default` | 利用者を置くテナント |
| `tenant_claim` | なし | テナントを決めるクレームの名前（`tenant` とは同時に使えません） |
| `role_mappings` | `[]` | `{"claim", "value", "role"}` の一覧。クレームが値を含めば、その役割（`admin`・`operator`・`viewer`）か権限 |
| `default_role` | なし | どの対応にも当たらないときの役割。無ければログインを断ります |
| `sync_roles` | `true` | ログインのたびに、役割を `role_mappings` から決め直します（`role_mappings` があるときだけ） |
| `auto_provision` | `false` | 初めてログインした人の利用者を自動で作ります |
| `username_claim` | `preferred_username` | 自動で作る利用者の名前に使うクレーム（無ければ `email`、`sub`） |
| `prompt`・`max_age` | なし | 認可の要求にそのまま付けます（`max_age` を付けると `auth_time` も確かめます） |

起動オプションは `--oidc-id`・`--oidc-display-name`・`--oidc-issuer`・`--oidc-client-id`・`--oidc-client-secret-file`・`--oidc-scopes`・`--oidc-redirect-url`・
`--oidc-allowed-domain`（繰り返し）・`--oidc-tenant`・`--oidc-tenant-claim`・`--oidc-role-map CLAIM=VALUE:ROLE`（繰り返し）・`--oidc-default-role`・
`--oidc-no-sync-roles`・`--oidc-auto-provision`・`--oidc-allow-unverified-email`・`--oidc-username-claim`・`--oidc-prompt`・`--oidc-max-age` です（[3. 設定](03-configuration.md)）。

### 利用者の決まり方

1. IdP の利用者（ID トークンの `sub`）と結び付いた SENCA の利用者がいれば、その利用者です。
2. いなければ、同じメールアドレスの利用者を探して結び付けます（メールアドレスが確認済みのときだけ）。管理者があらかじめメールアドレス付きで
   利用者を作っておけば、その人が初めて OIDC でログインしたときに結び付きます。
3. それでもいなければ、`auto_provision: true` なら利用者を作ります（パスワードは持ちません）。`false` ならログインを断ります（`error=no_account`）。

- `role_mappings` があると、ログインのたびに役割を決め直します。ネストしたクレームは `.` でつなぎます（例: Keycloak の `realm_access.roles`）。
  IdP の設定で `node:admin` を渡すこともできます。
- `role_mappings` が無ければ、自動で作るときに `default_role` を使い、その後の役割は SENCA の管理画面・API で変えます。
- 失敗したときの `error` の値（`email_domain_not_allowed`・`no_role`・`account_disabled` など）の一覧は、OpenAPI の `GET /auth/oidc/{provider}/callback` にあります。

### ID トークンの検証

- 取得するのは、設定した発行者の discovery と、そこに書かれた JWKS・トークンエンドポイントだけです（トークンの中の URL へは接続しません）。
  discovery の `issuer` が設定と違う場合や、発行者が `https://` なのに `http://` の宛先が書かれている場合は使いません。
- discovery と JWKS は 1 時間キャッシュします。知らない `kid` のトークンが来たら JWKS を取得し直します（鍵のローテーションに追従します）。
- 確かめる項目: 署名（EdDSA・ES256・RS256。アルゴリズムは鍵の種類で決まり、`none` と HS 系は使えません）・`iss`・`aud`（`client_id` を含む）・`exp`・`nbf`・
  `iat`（10 分以内）・`nonce`・`azp`（ある場合と `aud` が複数の場合）・`sub`。時計のずれは 60 秒まで許します。
- 発行者は管理者が設定した宛先なので、[12.6](#126-外部への接続の制限ssrf-対策) の宛先の検査（`--egress-allow`）は行いません。社内のネットワークの
  Keycloak などにも、許可を足さずに接続できます。

### Google（Google Workspace）

1. Google Cloud コンソールの「API とサービス」→「認証情報」で、OAuth クライアント ID（種類は「ウェブ アプリケーション」）を作ります。
   「承認済みのリダイレクト URI」に `https://senca.example.com/api/v1/auth/oidc/google/callback` を登録します。
2. クライアントシークレットをファイルに保存します。
3. Google の ID トークンにはグループの情報が入らないため、ドメインで利用者を絞り、役割は SENCA で割り当てます。

```json
{"providers": [{
  "id": "google", "display_name": "Google でログイン",
  "issuer": "https://accounts.google.com",
  "client_id": "1234567890-abcdefg.apps.googleusercontent.com",
  "client_secret_file": "/etc/senca/google-client-secret",
  "allowed_email_domains": ["example.com"],
  "auto_provision": true, "default_role": "viewer"
}]}
```

管理者にする人は、先に `POST /api/v1/users`（`{"username": "alice", "email": "alice@example.com", "roles": ["admin"]}`）で作っておけば、
初めてのログインでその利用者に結び付きます。

### Microsoft Entra ID

1. Entra 管理センターの「アプリの登録」で新規登録し、プラットフォーム「Web」のリダイレクト URI に
   `https://senca.example.com/api/v1/auth/oidc/entra/callback` を登録します。「証明書とシークレット」でクライアントシークレットを作ります。
2. 役割をグループで決める場合は、「トークン構成」で「グループ要求」を追加します（セキュリティグループ、ID トークンにはグループの ID）。
   `groups` クレームにはグループのオブジェクト ID が入ります。
3. 発行者は、テナントごとの `https://login.microsoftonline.com/{テナント ID}/v2.0` を使います（`common`・`organizations` は使えません）。

```json
{"providers": [{
  "id": "entra", "display_name": "Microsoft でログイン",
  "issuer": "https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0",
  "client_id": "11111111-2222-3333-4444-555555555555",
  "client_secret_file": "/etc/senca/entra-client-secret",
  "auto_provision": true,
  "role_mappings": [
    {"claim": "groups", "value": "aaaaaaaa-0000-0000-0000-000000000001", "role": "admin"},
    {"claim": "groups", "value": "aaaaaaaa-0000-0000-0000-000000000002", "role": "operator"}
  ],
  "default_role": "viewer"
}]}
```

- Entra ID の ID トークンには `email_verified` が入らず、`email` は利用者が変えられる場合があります。そのため既定では、メールアドレスによる結び付けと
  `allowed_email_domains` は働きません（ドメインを制限すると `error=email_not_verified` になります）。上の例のように、テナントごとの発行者で
  組織の利用者だけに絞り、グループで役割を決めてください。メールアドレスを組織が管理していて、既存の利用者に結び付けたい場合だけ
  `"require_email_verified": false` にします。
- 利用者が多くのグループ（ID トークンでは 200 を超えるもの）に属していると、`groups` クレームが省略されます。その場合はアプリに割り当てたグループだけを
  出す設定にしてください。

### Keycloak

1. レルムでクライアントを作ります（Client authentication を ON、Standard flow を ON）。「Valid redirect URIs」に
   `https://senca.example.com/api/v1/auth/oidc/keycloak/callback` を登録し、「Credentials」のシークレットをファイルに保存します。
2. 役割をグループで決める場合は、クライアントスコープに「Group Membership」のマッパーを追加します（Token Claim Name は `groups`、
   Full group path は OFF、Add to ID token は ON）。レルムのロールを使う場合は、「roles」スコープの「realm roles」マッパーで
   Add to ID token を ON にします（クレームは `realm_access.roles`）。

```json
{"providers": [{
  "id": "keycloak", "display_name": "社内アカウント",
  "issuer": "https://sso.example.com/realms/media",
  "client_id": "senca",
  "client_secret_file": "/etc/senca/keycloak-client-secret",
  "auto_provision": true,
  "role_mappings": [
    {"claim": "groups", "value": "senca-admins", "role": "admin"},
    {"claim": "realm_access.roles", "value": "senca-operator", "role": "operator"}
  ],
  "default_role": "viewer"
}]}
```

Keycloak のメールアドレスは、レルムの「Email verified」が ON の利用者だけが確認済みになります。

### Auth0 など

- Auth0 の発行者は `https://{ドメイン}/`（最後の `/` を含みます）です。アプリケーションの種類は「Regular Web Application」にし、
  「Allowed Callback URLs」にリダイレクト URI を登録します。
- その他の IdP も、discovery（`/.well-known/openid-configuration`）・認可コードフロー・PKCE の S256 に対応していれば使えます。
  ID トークンの署名は EdDSA・ES256・RS256 のどれかにしてください。

### テナントとクレーム

複数の組織を 1 つの SENCA で扱う場合は、IdP のクレームでテナントを決められます（`"tenant_claim": "org"`）。
クレームの値は 1〜64 文字の英数字と `. _ -` でなければなりません。API トークンは作成者のテナントにしか作れないため、
新しいテナントの最初の管理者は、この方法か JWT で作ります。

## 12.16 ログインできなくなったとき

- **他の管理者がいる場合**: その管理者が `POST /api/v1/users/{id}/password`（パスワードの再設定）や `DELETE /api/v1/users/{id}/mfa`（MFA を外す）を使います。
- **管理者トークンがある場合**: ノードのサーバーで次のコマンドを実行すると、利用者を有効に戻し、MFA を外して、仮のパスワードを表示します。
  次のログインでパスワードの変更を求めます。

  ```sh
  senca-node reset-password --data-dir /var/lib/senca --username alice
  # temporary password for alice: Xk3...（20 文字）
  ```

  動いているノードの管理 API（既定は `http://127.0.0.1:8080`、`--api` で変更）を、`{data-dir}/control/bootstrap-admin-token`
  （無ければ `recovery-admin-token`）のトークンで呼びます。別のファイルのトークンは `--token-file` で渡します。利用者がいなければ、
  `--create-admin` で管理者を作れます。API を通すので、クラスターでもそのまま使えます。
- **管理者トークンもない場合**（最初の管理者トークンを削除したか、値をなくした）: ノードを 1 台、`--recovery-admin-token` を付けて起動し直します。
  ノードは `control/recovery-admin-token`（0600）に、`default` テナントの管理者トークン（`tenant:admin`・`node:admin`、24 時間で失効）を書き出します。
  このトークンで上の `reset-password` を実行し、終わったらファイルを削除して、オプションを外して起動し直してください（ファイルがある間は新しいトークンを作りません）。
  データディレクトリと起動の引数を変えられる人は、もともと制御状態のすべてを読めるため、この方法で権限が増えることはありません。
