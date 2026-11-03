# 3. 設定

`senca-node` の設定は、コマンドラインのオプションか、`--config FILE` で指定する JSON ファイルで行います。
このページでは、すべてのオプション、設定ファイルの形式、クラスターの設定、環境変数、データディレクトリの内容を説明します。

- 関連するページ: [2. クイックスタート](02-quickstart.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
- オプションの一覧は `senca-node --help` でも表示できます。

## 3.1 基本的な規則

- オプションは `--name value` の形で指定します（`--name=value` の形は使えません）。値を取らないオプションは
  `--capture-synthetic`・`--verify-recordings`・`--log-json`・`--rooms-cascade` です。
- 未知のオプションを指定するとエラーになります（終了コード 2、使い方を表示）。設定が無効な場合は起動しません（一部の設定だけを反映して起動することはありません）。
- `--config FILE` を指定すると、まずその JSON を読み込み、その後でコマンドラインのオプションを（指定した位置に関係なく）上書きとして適用します。
- 待ち受けアドレスは `IP:ポート` の形で指定します（例: `0.0.0.0:1935`、`[::]:8080`）。ホスト名は使えません。
  `--http` 以外の待ち受けは、`off` を指定すると無効にできます。
- `--help`（`-h`）は使い方を表示して終了します。`--healthcheck URL` は通常の起動とは別の使い方です（最初の引数として指定します。
  [13. 運用](13-operations.md#132-ヘルスチェック) を参照）。

## 3.2 オプション

### 待ち受け

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--http` | ADDR | `0.0.0.0:8080` | HTTP。管理 API（`/api/v1`）・SSE・署名付きの HLS（`/s/...`）・WHIP/WHEP・シグナリング（`/signaling`）・管理画面・`/healthz`・`/readyz` を提供します。無効にはできません |
| `--https` | ADDR\|`off` | 無効 | HTTPS（HTTP と同じ内容）。`--tls-cert`・`--tls-key` が必要です |
| `--rtmp` | ADDR\|`off` | `0.0.0.0:1935` | RTMP（publish・play） |
| `--rtmps` | ADDR\|`off` | 無効 | RTMPS。`--tls-cert`・`--tls-key` が必要です |
| `--srt` | ADDR\|`off` | 無効 | SRT（UDP） |
| `--rtsp` | ADDR\|`off` | 無効 | RTSP の play（TCP） |
| `--webrtc-udp` | ADDR\|`off` | 無効 | WebRTC の UDP ソケット（WHIP・WHEP・部屋で共用）。`0.0.0.0:ポート` で待つと、ICE の候補にはノードのネットワークインターフェースのアドレス（ループバックを除く）を載せます。決まったアドレスで待つと、そのアドレスを載せます |
| `--webrtc-public-ip` | IP[,IP…] | なし | ICE の候補に載せる IP アドレス（ポートは `--webrtc-udp` のもの。8 個まで、繰り返し指定できます）。NAT・Docker のように、相手から見えるアドレスが待ち受けのアドレスと違うときに指定します |
| `--quic` | ADDR\|`off` | 無効 | QUIC v1 の UDP ポートです。HTTP/3（ALPN `h3`。SSE と `/signaling` を除き HTTP と同じ内容）、WebTransport、MoQ draft-22（ALPN `moqt-22`。WebTransport では `/moq`）を 1 つのポートで受け付けます。`--tls-cert`・`--tls-key` が必要です。有効にすると、HTTP・HTTPS の応答に `Alt-Svc: h3=":ポート"` が付きます（[4. 取り込みと再生](04-ingest-playback.md#415-moq-と-http3)） |
| `--tls-cert` | FILE | なし | `--https`・`--rtmps`・`--quic` で使う証明書チェーン（PEM。先頭がサーバー証明書） |
| `--tls-key` | FILE | なし | 上記の証明書の秘密鍵（PEM。サーバー証明書の公開鍵と一致しない場合は起動しません） |

### 公開 URL

API の応答（`ingest`）や再生 URL に含める、外部から見た URL です。待ち受けアドレスとは別に指定します。

| オプション | 既定 | 使われる場所 |
|---|---|---|
| `--public-http-url` | `http://127.0.0.1:8080` | 再生 URL の `hls`・`whep`、`ingest.whip_url` |
| `--public-rtmp-url` | `rtmp://127.0.0.1:1935` | `ingest.rtmp_url`（`{URL}/live`）、再生 URL の `rtmp`（`{URL}/play/{id}?...`） |
| `--public-srt-url` | なし | `ingest.srt_url`（指定した値をそのまま使用）。指定しない場合、`srt_url` は出力されません |
| `--public-rtsp-url` | なし | 再生 URL の `rtsp`（`{URL}/live/{id}?...`）。指定しない場合は出力されません |

ポート番号を既定から変えた場合や、リバースプロキシやコンテナの外からアクセスする場合は、必ずこれらを合わせてください。

### 再生・録画

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--max-playback-ttl` | 秒 | `86400` | 再生 URL の `ttl_secs` の上限（ライブ・録画の両方）。0 は指定できません |
| `--reconnect-grace` | ミリ秒 | `30000` | 配信者の接続が切れてから、再接続を同じ配信の続きとして待つ時間 |
| `--record-queue` | N | `4096` | 録画の書き込みキューのサイズの基準値（16〜1048576）。ストリームごとのキューは N/64 件です（16〜4096 の範囲に丸めます。既定では 64 セグメント分）。キューからあふれた分は録画の欠落になります |
| `--retention-sweep` | 秒 | `60` | 保持期限を過ぎた録画を削除する定期処理の間隔（Idempotency-Key の記録の期限切れ処理も同時に行います）。最小 1 |
| `--verify-recordings` | — | 無効 | 起動時の復旧処理で、インデックスにあるすべてのセグメントの SHA-256 を照合し、一致しないものをログに出力します（データ量に比例して起動が遅くなります） |
| `--zoneinfo` | DIR | `/usr/share/zoneinfo` | 予約録画のタイムゾーン（IANA の名前）を読み込む TZif ファイルのディレクトリ |

### 認証（JWT）

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--jwks` | FILE | なし | JWT の検証に使う鍵（JWKS の JSON、ローカルのファイル）。起動時に 1 回だけ読み込みます |
| `--jwt-issuer` | 文字列 | なし | `iss` と一致する必要がある値。`--jwks` を使う場合は必須です |
| `--jwt-audience` | 文字列 | なし | `aud` に含まれている必要がある値。`--jwks` を使う場合は必須です |
| `--jwt-tenant-claim` | 名前 | `tenant` | テナントを取り出すクレームの名前 |

詳しくは [12. セキュリティ](12-security.md) を参照してください。

### コーデック・キャプチャ

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--codec-workers` | N | `0`（= CPU 数） | 単発の処理（静止画・サムネイルなど）に使う codec worker のプロセス数 |
| `--codec-worker-bin` | FILE | `senca-node` と同じディレクトリの `senca-codec-worker` | codec worker の実行ファイル。見つからない場合、変換は 503 になります |
| `--codec-lib-dir` | DIR | なし | コーデックの共有ライブラリを最初に探すディレクトリ。codec worker と capture worker に `SENCA_CODEC_LIB_DIR` として渡します（`:` 区切りで複数指定可）。[2.3](02-quickstart.md#23-コーデックのライブラリ) を参照 |
| `--capture-synthetic` | — | 無効 | テスト用の合成キャプチャデバイスを表示します（capture worker に `--synthetic` を渡します） |

### 管理画面・配信ページ

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--console-dir` | DIR | なし | 管理画面（`apps/console`）を `/console/` で配信します。指定しない場合は 404 |
| `--sender-dir` | DIR | なし | ブラウザーから配信するためのページ（`apps/sender-web`）を `/sender/` で配信します。指定しない場合は 404 |

`/` にアクセスすると、`/console/` へ 302 でリダイレクトします。

### ノード・グリッド

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--node-name` | 名前 | `node-1` | ノードの表示名。グリッドのラベル `senca.io/name` になります。デバイスの API では、ノード ID の代わりに使えます |
| `--label` | KEY=VALUE | なし | 配置の条件（`placement.labels`）に使うラベル。複数回指定できます。キーは 1〜128 文字、値は 128 文字まで、制御文字は使えません。`senca.io/` で始まるキーと `raft_id` は予約されています。最大 64 個 |
| `--net-mbps` | Mbit/s | `10000` | このノードの帯域（上り・下りそれぞれ）。グリッドの配置計算に使います |
| `--capacity-cpu-millis` | ミリコア | CPU 数 × 1000 × 90% | ワークロードに割り当てる CPU |
| `--capacity-memory-mb` | MB | `MemTotal` の 85% | ワークロードに割り当てるメモリ |
| `--cluster-config` | FILE | なし | クラスターの設定（JSON、[3.4](#34-クラスターの設定)）。指定しない場合は単一ノードで動作します |
| `--egress-allow` | CIDR | なし | 外部への接続（pull 入力・push 出力・字幕・PaaS・HLS pull）の宛先として許可するプライベートアドレスの範囲。複数回指定できます。[12. セキュリティ](12-security.md) を参照 |

### 部屋（リアルタイム）

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--rooms-cascade` | — | 無効 | すべての部屋でカスケードを使います（部屋の設定 `cascade` が false でも、別のノードにある部屋の参加者をこのノードで受け付けます）。[10. リアルタイムの部屋](10-rooms.md) を参照 |
| `--rooms-auth-webhook` | URL | なし | 部屋の認証 Webhook（部屋の設定 `auth_webhook_url` が優先されます）。`--rooms-webhook-secret-file` が必要です |
| `--rooms-auth-mode` | `after_token`・`only` | `after_token` | `after_token` はトークンを検証した後に Webhook に問い合わせます。`only` はトークンの代わりに Webhook で参加を認めます |
| `--rooms-event-webhook` | URL | なし | 部屋のイベントを送信する Webhook（署名付き、再送あり）。`--rooms-webhook-secret-file` が必要です |
| `--rooms-webhook-secret-file` | FILE | なし | Webhook の署名（HMAC-SHA256）に使う秘密を書いたファイル。前後の空白を除いて 16 バイト以上必要です |

Webhook の宛先は、`--egress-allow` のポリシーで検査します（プライベートアドレスへ送るには許可が必要です）。

### 要求の量の上限

上限を超えた要求は 429（`RATE_LIMITED`、`Retry-After` 付き）になります（[12. セキュリティ](12-security.md)）。どれも `0` で無効です。

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--rate-limit-ip` | N | `300` | 管理 API（`/api/v1`）の、送信元の IP アドレスごとの 1 秒あたりの要求数（一時的には 2 倍まで）。認証の前に数えます |
| `--rate-limit-token` | N | `200` | 管理 API の、API トークン（JWT は subject）ごとの 1 秒あたりの要求数（一時的には 2 倍まで） |
| `--rate-limit-auth-failures` | N | `30` | 認証に失敗してよい回数（送信元の IP アドレスごと、1 分あたり）。使い切ると、正しいトークンでも 429 になります |
| `--rate-limit-connect` | N | `30` | 接続の開始（`/signaling`・WHIP・WHEP・SSE）の、送信元の IP アドレスごとの 1 秒あたりの数（一時的には 2 倍まで） |

### その他

| オプション | 値 | 既定 | 説明 |
|---|---|---|---|
| `--config` | FILE | なし | 設定ファイル（JSON、[3.3](#33-設定ファイルjson)） |
| `--data-dir` | DIR | `out`（カレントディレクトリからの相対パス） | データディレクトリ（[3.6](#36-データディレクトリ)）。必ず指定してください |
| `--log-json` | — | 無効 | ノードのログを 1 行 1 JSON の形式で標準出力に出力します（[13. 運用](13-operations.md)） |
| `--events-retain` | N | `4096` | SSE（`GET /api/v1/events`）でつなぎ直したときに再送するため、ノードが残しておく直近のイベントの数（0〜1000000）。[15. API と SDK](15-api-and-sdks.md) を参照 |

### 起動時の検査

次の場合は起動しません（終了コード 2）。

- `--jwks` を指定したのに、`--jwt-issuer` または `--jwt-audience` がない。
- `--https`・`--rtmps`・`--quic` を指定したのに、`--tls-cert` または `--tls-key` がない。
- `--record-queue` が 16〜1048576 の範囲外。`--max-playback-ttl` が 0。
- `--egress-allow` を CIDR として解釈できない。
- `--webrtc-public-ip` が IP アドレスではない、`0.0.0.0`・`::`・マルチキャストのアドレス、または 9 個以上。`--events-retain` が 1000000 を超える。
- `--rooms-auth-webhook`・`--rooms-event-webhook` が http/https の URL ではない、または `--rooms-webhook-secret-file` がない。
  秘密のファイルを読み込めない、または 16 バイト未満。
- クラスターの設定の `node_id` が 0、`peers` に自分が含まれていない、`peers` が 1〜7 台の範囲外。

ファイルを読み込めない、待ち受けを開始できない、クラスターの共有シークレットが 16 文字未満、などの場合は、起動の途中で
`fatal: ...` と出力して終了コード 1 で終了します。

## 3.3 設定ファイル（JSON）

`--config FILE` で指定する JSON は、次のキーを持つオブジェクトです。どのキーも省略でき、省略したものは既定値になります。
**未知のキーはエラーになります。** コマンドラインのオプションとの対応は次のとおりです。

| JSON のキー | オプション | 型 |
|---|---|---|
| `data_dir` | `--data-dir` | 文字列 |
| `node_name` | `--node-name` | 文字列 |
| `http_listen` | `--http` | 文字列（ADDR） |
| `https_listen`・`rtmp_listen`・`rtmps_listen`・`srt_listen`・`rtsp_listen`・`webrtc_udp`・`quic_listen` | `--https` など（`quic_listen` は `--quic`） | 文字列（ADDR）または `null`（無効） |
| `tls_cert`・`tls_key` | `--tls-cert`・`--tls-key` | 文字列 |
| `public` | `--public-*-url` | `{"rtmp": 文字列, "http": 文字列または null, "srt": 文字列または null, "rtsp": 文字列または null}` |
| `max_playback_ttl_secs` | `--max-playback-ttl` | 数値 |
| `reconnect_grace_ms` | `--reconnect-grace` | 数値 |
| `record_queue` | `--record-queue` | 数値 |
| `retention_sweep_secs` | `--retention-sweep` | 数値 |
| `jwks`・`jwt_issuer`・`jwt_audience`・`jwt_tenant_claim` | `--jwks` など | 文字列 |
| `codec_workers` | `--codec-workers` | 数値 |
| `codec_worker_bin`・`codec_lib_dir` | 同名のオプション | 文字列 |
| `zoneinfo_dir` | `--zoneinfo` | 文字列 |
| `console_dir`・`sender_dir` | 同名のオプション | 文字列 |
| `capture_synthetic`・`verify_recordings`・`log_json` | 同名のオプション | 真偽値 |
| `cluster` | `--cluster-config` | オブジェクト（[3.4](#34-クラスターの設定)） |
| `egress_allow` | `--egress-allow` | 文字列の配列 |
| `net_mbps` | `--net-mbps` | 数値 |
| `labels` | `--label` | 文字列 → 文字列のオブジェクト |
| `capacity_cpu_millis`・`capacity_memory_mb` | 同名のオプション | 数値または `null` |
| `rooms_cascade` | `--rooms-cascade` | 真偽値 |
| `rooms_auth_webhook`・`rooms_event_webhook` | 同名のオプション | 文字列または `null` |
| `rooms_auth_mode` | `--rooms-auth-mode` | `"after_token"` または `"only"` |
| `rooms_webhook_secret_file` | 同名のオプション | 文字列または `null` |

注意点:

- JSON で `rtmp_listen` を省略すると、既定値（`0.0.0.0:1935`）になります。無効にする場合は `null` を指定してください。
- `public` の中で省略したキーは既定値になります（`rtmp`・`http` は [3.2](#公開-url) の既定値、`srt`・`rtsp` はなし）。
  `"http": null` を明示すると、再生 URL の `hls` は `/s/...` から始まる相対パスになり、`ingest.whip_url` は出力されません。
- `labels` も `--label` と同じ規則で検査します（規則に合わない場合は起動せず、終了コード 2 で終了します）。

例:

```json
{
  "data_dir": "/var/lib/senca",
  "node_name": "edge-1",
  "http_listen": "0.0.0.0:8080",
  "rtmp_listen": "0.0.0.0:1935",
  "srt_listen": "0.0.0.0:9000",
  "public": {
    "rtmp": "rtmp://media.example.com:1935",
    "http": "https://media.example.com",
    "srt": "srt://media.example.com:9000"
  },
  "codec_lib_dir": "/usr/lib/x86_64-linux-gnu",
  "labels": { "zone": "a" },
  "log_json": true
}
```

## 3.4 クラスターの設定

クラスターの設定は、`--cluster-config FILE`（または `--config` の `cluster`）で指定します。未知のキーはエラーになります。
クラスターの使い方は [9. クラスタとグリッド](09-cluster-grid.md) を参照してください。

### ClusterConfig

| キー | 必須 | 型 | 説明 |
|---|---|---|---|
| `node_id` | はい | 数値（1 以上） | このノードの Raft の ID。`peers` の中に同じ `id` が必要です |
| `listen` | はい | ADDR | ノード間通信（Raft）の待ち受けアドレス |
| `peers` | はい | 配列（1〜7） | 投票に参加するノード（自分を含む） |
| `secret_file` | はい | パス | クラスターの共有シークレットのファイル。全ノードで同じ内容にします。前後の空白を除いて 16 文字以上必要です |
| `tls_ca` | いいえ | パス | ノード間の mTLS に使う認証局の証明書（PEM） |
| `tls_cert` | いいえ | パス | このノードの証明書（PEM。SAN に `DNS:senca-cluster` が必要） |
| `tls_key` | いいえ | パス | 上記の証明書の秘密鍵（PEM） |
| `relay_listen` | いいえ | ADDR | ノード間の中継の待ち受けアドレス。既定は `listen` のポート + 1 |

`tls_ca`・`tls_cert`・`tls_key` は、3 つすべてを指定するか、すべて省略してください。省略した場合、ノード間の通信は平文になります
（[12. セキュリティ](12-security.md#127-クラスタの-mtls)）。

### ClusterPeer（`peers[]`）

| キー | 必須 | 型 | 説明 |
|---|---|---|---|
| `id` | はい | 数値 | そのノードの Raft の ID |
| `addr` | はい | ADDR | そのノードの Raft のアドレス |
| `api` | いいえ | URL | そのノードの公開 HTTP の URL（例: `http://10.0.0.2:8080`）。視聴者を 307 でリダイレクトするときと、部屋の参加者を別のノードへ案内するときに使います |
| `relay` | いいえ | ADDR | そのノードの中継のアドレス。既定は `addr` のポート + 1 |

中継のポートの既定値は「Raft のポート + 1」なので、そのポート番号は空けておいてください。1 台のマシンで複数のノードを動かす場合（テストなど）は、
`relay_listen` と `peers[].relay` を明示してください。いずれかのノードの中継アドレスが、別のノードの Raft または中継のアドレスと
重なる設定（例: 同じホストで Raft を 7000・7001 にし、中継を既定のままにした場合）は、起動時の検査でエラーになります
（終了コード 2。重なったアドレスがメッセージに表示されます）。

```json
{
  "node_id": 1,
  "listen": "10.0.0.1:7000",
  "secret_file": "/etc/senca/cluster-secret",
  "tls_ca": "/etc/senca/cluster-ca.pem",
  "tls_cert": "/etc/senca/n1.pem",
  "tls_key": "/etc/senca/n1.key",
  "peers": [
    {"id": 1, "addr": "10.0.0.1:7000", "api": "http://10.0.0.1:8080"},
    {"id": 2, "addr": "10.0.0.2:7000", "api": "http://10.0.0.2:8080"},
    {"id": 3, "addr": "10.0.0.3:7000", "api": "http://10.0.0.3:8080"}
  ]
}
```

## 3.5 環境変数

`senca-node` 自身が読む環境変数はありません。設定はすべて、オプションか JSON で指定します。
次の環境変数は、ノードが起動する worker が読み込みます。

| 変数 | 読み込むプロセス | 説明 |
|---|---|---|
| `SENCA_CODEC_LIB_DIR` | codec worker | コーデックのライブラリを最初に探すディレクトリ（`:` 区切り）。`--codec-lib-dir` を指定した場合は、その値で置き換えられます |
| `SENCA_CODEC_DISABLE` | codec worker | 使わないプロバイダー（`,` 区切り）。`hardware`・`software`、または `nvidia`・`intel-vpl`・`amd-amf`・`vaapi`・`dav1d`・`libvpx`・`svt-av1`・`opus` |
| `LD_LIBRARY_PATH` | codec worker | ファイル名だけで `dlopen` するときの、OS の検索パス |
| `SENCA_CAPTURE_SYNTHETIC` | capture worker | `1`・`true` でテスト用のデバイスを表示します |
| `SENCA_CAPTURE_DISABLE` | capture worker | 検出しないバックエンド（`,` 区切り: `v4l2`・`alsa`・`decklink`） |
| `SENCA_CAPTURE_LIB_DIR` | capture worker | DeckLink・ALSA のライブラリを探すディレクトリ（`:` 区切り） |

- codec worker は環境変数を空にした状態で起動し、上記の 3 つ（`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH`）
  だけを親プロセスから引き継ぎます。
- capture worker は、親プロセスの環境変数をすべて引き継ぎます。
- `compose.yaml` の `SENCA_PUBLIC_HTTP`・`SENCA_PUBLIC_RTMP`・`SENCA_PUBLIC_IP` は compose の変数で、`--public-*-url`・`--webrtc-public-ip` の値として展開されます。
  ノード自身はこれらを読みません。

## 3.6 データディレクトリ

`--data-dir` の内容は次のとおりです。必要なファイルやディレクトリは、ノードが自動で作成します。

| パス | 内容 |
|---|---|
| `node-id` | このノードの ID（128 bit、16 進数）。初回の起動時に作成し、以後は変わりません |
| `audit.log` | 監査ログ（1 行 1 JSON、追記） |
| `control/log` | 単一ノードでの制御状態の追記ログ（1 件ごとに fsync） |
| `control/snapshot.json` | 単一ノードでの制御状態のスナップショット。ログが 1000 件を超えると作り直し、ログを空にします |
| `control/raft/` | クラスターでの Raft のデータ（`hardstate`・`snap-*.snap`・`log/*.seg`）。クラスターでは `control/log` は使いません |
| `control/secret-key` | 秘密値を暗号化する鍵（32 バイト、0600）。なければ初回に作成します。クラスターでは、いずれか 1 台の鍵がクラスター共通の鍵になり、クラスターの共有シークレットで暗号化して全ノードへ複製されます |
| `control/bootstrap-admin-token` | 最初の管理者トークンの値（0600）。初回の起動時にだけ作成します |
| `recordings/{録画 ID}/index.jsonl` | 録画のインデックス（追記のみの JSON Lines: `begin`・`init`・`segment`・`gap`・`end`） |
| `recordings/{録画 ID}/thumbnail.jpg`・`thumbnail.json` | 録画のサムネイルとその情報 |
| `recordings/.trash-{録画 ID}` | 削除処理中の録画（一時的なもの） |
| `segments/{ストリーム ID}/{run}/{連番}.ts`・`.m4s`（と fMP4 の初期化セグメント） | 録画のセグメント（作成後は変更しません）。`run` はストリームのタスクごとの ID です |
| `segments/{ストリーム ID}/orphans/{run}/` | 起動時の復旧処理で、どのインデックスからも参照されていないとわかったセグメント（削除せずに隔離します） |
| `snapshots/{ストリーム ID}/{ID}.jpg`・`{ID}.json` | 定期的に取得した静止画とその情報 |

- 単一ノードの制御状態（`control/log`・`snapshot.json`）には、再生 URL の署名鍵が平文で含まれています。
  データディレクトリのアクセス権は、[12. セキュリティ](12-security.md) を参照して制限してください。
- バックアップの対象については [13. 運用](13-operations.md) を参照してください。
