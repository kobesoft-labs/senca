# 12. セキュリティ

この章では、SENCA のセキュリティに関わる機能と設定を説明します。
管理 API の認証と権限、配信者と視聴者の資格情報（stream key・署名付き URL・参加トークン）、秘密値の保管、外部への接続の制限、
クラスターの mTLS、codec worker のサンドボックスを扱います。

- 関連するページ: [3. 設定](03-configuration.md)、[9. クラスターとグリッド](09-cluster-grid.md)、[10. リアルタイムの部屋](10-rooms.md)、[13. 運用](13-operations.md)、[15. 管理 API と SDK](15-api-and-sdks.md)
- 設計の背景: [ADR-0002 依存関係と暗号の部品](../../dev/adr/0002-dependencies-and-crypto.md)、[ADR-0007 入力・出力・秘密値と外部への接続の制限](../../dev/adr/0007-inputs-outputs-egress.md)、[ADR-0009 分散の制御プレーン](../../dev/adr/0009-cluster.md)

## 12.1 管理 API の認証

管理 API（`/api/v1/...`）と SSE（`/api/v1/events`）は、`Authorization: Bearer <値>` ヘッダーで認証します。

- 値が `senca_` で始まれば **API トークン**、それ以外は **JWT** として扱います（JWT は `--jwks` を指定したときだけ使えます）。
- 認証に失敗すると 401（`WWW-Authenticate: Bearer`）を返します。失敗の理由（未知のトークン、期限切れ、署名の誤りなど）は応答に含めず、
  ノードのログに `auth failed: <理由> request_id=...` として記録します。
- 認証なしで応答するのは、`/healthz`・`/readyz`、管理画面と配信用ページの静的ファイル、署名付きの再生 URL（`/s/...`）です。
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
| `publish` | （予約済み。現在の API でこの権限を必要とする操作はありません） |
| `subscribe` | ライブの再生 URL の発行 |
| `recording:read` | 録画の参照、録画の再生 URL・サムネイル・MP4 の書き出し |
| `recording:delete` | 録画の削除 |
| `tenant:admin` | `node:admin` 以外のすべて。加えて、トークンと秘密値の管理 |
| `node:admin` | テナントをまたぐ運用操作（`/nodes`・`/grid`・`/devices`、ノードの退避（drain）、デバイスの停止・再開、ノード・グリッドのイベント） |

操作ごとに必要な権限は、[15. 管理 API と SDK](15-api-and-sdks.md) と OpenAPI（[docs/api/openapi.yaml](../../api/openapi.yaml)）を見てください。

### テナント

すべてのリソースはテナントに属し、他のテナントのリソースは見えません（404 になります）。最初の管理者トークンのテナントは `default` です。
API トークンは作成者と同じテナントにしか作れないため、**別のテナントを作るには JWT を使います**（テナントは JWT のクレームで決まります）。

### 最初の管理者トークン

初回の起動時に、ノードは `data_dir/control/bootstrap-admin-token`（パーミッション 0600）に管理者トークンを書き出します
（`tenant:admin`・`node:admin`、期限なし）。クラスターでは、ID が最も小さいノードだけが作成します。
運用を始める前に、次の手順で入れ替えてください。

1. 必要な権限だけを持つ、期限付きのトークンを作ります。
2. 新しい管理者トークンを作ってから、最初のトークンを `DELETE /api/v1/tokens/{id}` で削除します
   （最後の `tenant:admin` トークンは削除できないため、先に新しいものを作ります）。
3. `bootstrap-admin-token` ファイルを削除するか、安全な場所へ移します（ノードはこのファイルを再び読みません）。

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

`subject` は、API トークンなら `token:{ID}`、JWT なら `sub` の値です。秘密情報やリクエストの本文は記録しません。
Idempotency-Key のために保存する応答からも、`stream_key`・`token`・`secret` を消してから保存します（再送時の応答では、これらは `null` になり、
`*_redacted: true` が付きます）。
