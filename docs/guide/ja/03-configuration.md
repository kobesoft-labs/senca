# 3. 設定

`senca-node` の設定は、コマンドラインのオプションと、`--config FILE`（JSON）で渡す。このページはすべてのオプション、
JSON の形、クラスタの設定、環境変数、データ置き場の中身をまとめる。

- 関係するページ: [2. クイックスタート](02-quickstart.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
- 典拠: `crates/senca-node/src/config.rs`（`senca-node --help` で一覧が出る）

## 3.1 決まり

- 書き方は `--name value`（`--name=value` の形は受け付けない）。値を取らないのは `--capture-synthetic`・
  `--verify-recordings`・`--log-json` だけ。
- 知らないオプションはエラー（終了コード 2、使い方を表示）。無効な設定では起動しない（一部だけ反映することはない）。
- `--config FILE` があれば、まずその JSON を読み、次にコマンドラインのオプションを（位置に関わらず）上書きとして適用する。
- 待ち受けのアドレスは `IP:ポート`（例 `0.0.0.0:1935`、`[::]:8080`）。ホスト名は書けない。
  `--http` 以外の待ち受けは `off` で無効にできる。
- `--help`（`-h`）で使い方を表示して終わる。`--healthcheck URL` は[別扱い](13-operations.md#132-ヘルスチェック)（最初の引数に書く）。

## 3.2 オプション

### 待ち受け

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--http` | ADDR | `0.0.0.0:8080` | HTTP。管理 API（`/api/v1`）・SSE・署名付きの HLS（`/s/...`）・WHIP/WHEP・シグナリング（`/signaling`）・管理画面・`/healthz`・`/readyz`。無効にはできない |
| `--https` | ADDR\|`off` | 無効 | HTTPS（HTTP と同じ内容）。`--tls-cert`・`--tls-key` が要る |
| `--rtmp` | ADDR\|`off` | `0.0.0.0:1935` | RTMP（publish・play） |
| `--rtmps` | ADDR\|`off` | 無効 | RTMPS。`--tls-cert`・`--tls-key` が要る |
| `--srt` | ADDR\|`off` | 無効 | SRT（UDP） |
| `--rtsp` | ADDR\|`off` | 無効 | RTSP の play（TCP） |
| `--webrtc-udp` | ADDR\|`off` | 無効 | WebRTC の UDP ソケット（WHIP・WHEP・部屋で共用）。このアドレスがそのまま ICE の候補になるので、相手から届く IP を書く（`0.0.0.0` は使わない） |
| `--tls-cert` | FILE | なし | `--https`・`--rtmps` の証明書チェーン（PEM、先頭が leaf） |
| `--tls-key` | FILE | なし | 上の秘密鍵（PEM。leaf の公開鍵と一致しないと起動しない） |

### 公開 URL

API の応答（`ingest`）と再生 URL に入れる、外から見た URL。待ち受けのアドレスとは別に指定する。

| オプション | 既定 | 使われる所 |
|---|---|---|
| `--public-http-url` | `http://127.0.0.1:8080` | 再生 URL の `hls`・`whep`、`ingest.whip_url` |
| `--public-rtmp-url` | `rtmp://127.0.0.1:1935` | `ingest.rtmp_url`（`{URL}/live`）、再生 URL の `rtmp`（`{URL}/play/{id}?...`） |
| `--public-srt-url` | なし | `ingest.srt_url`（書いた値そのまま）。無ければ `srt_url` を出さない |
| `--public-rtsp-url` | なし | 再生 URL の `rtsp`（`{URL}/live/{id}?...`）。無ければ出さない。`--help` の一覧には載っていない |

ポートを既定から変えたとき、リバースプロキシやコンテナの外から見るときは、必ず合わせる。

### 再生・録画

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--max-playback-ttl` | 秒 | `86400` | 再生 URL の `ttl_secs` の上限（ライブ・録画とも）。0 は不可 |
| `--reconnect-grace` | ミリ秒 | `30000` | 送り手が切れてから stream を「同じ配信の続き」として待つ時間 |
| `--record-queue` | N | `4096` | 録画の書き手のキューの大きさの元の値（16〜1048576）。stream ごとのキューは N/64 件（16〜4096 に丸める。既定で 64 セグメント分）。あふれた分は録画の欠落になる |
| `--retention-sweep` | 秒 | `60` | 保持期限を過ぎた録画を消す見回りの間隔（Idempotency-Key の記録の期限切れも同時）。最小 1 |
| `--verify-recordings` | — | 無効 | 起動時の回収で、索引にあるすべてのセグメントの SHA-256 を照合し、合わないものをログに出す（データ量に比例して起動が遅くなる） |
| `--zoneinfo` | DIR | `/usr/share/zoneinfo` | 予約の時間帯（IANA の名前）を読む TZif の置き場所 |

### 認証（JWT）

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--jwks` | FILE | なし | JWT を検証する鍵（JWKS の JSON、ローカルのファイル）。起動時に 1 回だけ読む |
| `--jwt-issuer` | 文字列 | なし | `iss` と一致を求める値。`--jwks` には必須 |
| `--jwt-audience` | 文字列 | なし | `aud` に含まれることを求める値。`--jwks` には必須 |
| `--jwt-tenant-claim` | 名前 | `tenant` | テナントを取り出すクレームの名前 |

詳しくは [12. セキュリティ](12-security.md)。

### コーデック・キャプチャ

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--codec-workers` | N | `0`（= CPU の数） | 単発の仕事（静止画・サムネイルなど）用の codec worker のプロセス数 |
| `--codec-worker-bin` | FILE | `senca-node` と同じディレクトリの `senca-codec-worker` | codec worker の実行ファイル。見つからなければ変換は 503 |
| `--codec-lib-dir` | DIR | なし | コーデックの共有ライブラリを最初に探すディレクトリ。worker と capture worker に `SENCA_CODEC_LIB_DIR` として渡す（`:` 区切りで複数可）。[2.3](02-quickstart.md#23-コーデックのライブラリ) |
| `--capture-synthetic` | — | 無効 | 試験用の合成のキャプチャ機器を出す（capture worker に `--synthetic` を渡す）。`--help` の一覧には載っていない |

### 画面

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--console-dir` | DIR | なし | 管理画面（`apps/console`）を `/console/` で配る。無ければ 404 |
| `--sender-dir` | DIR | なし | ブラウザーからの送出のページ（`apps/sender-web`）を `/sender/` で配る。無ければ 404 |

`/` は `/console/` へ 302 で案内する。

### ノード・グリッド

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--node-name` | 名前 | `node-1` | ノードの表示名。グリッドのラベル `senca.io/name` になる。装置の API ではノード ID の代わりに使える |
| `--label` | KEY=VALUE | なし | 配置の条件（`placement.labels`）に使うラベル。何度でも書ける。キーは 1〜128 文字、値は 128 文字まで、制御文字は不可、`senca.io/` で始まるキーと `raft_id` は予約、64 個まで |
| `--net-mbps` | Mbit/s | `10000` | このノードの帯域（上り・下りそれぞれ）。グリッドの配置で数える |
| `--capacity-cpu-millis` | ミリコア | CPU の数 × 1000 × 90% | 仕事に貸す CPU |
| `--capacity-memory-mb` | MB | `MemTotal` の 85% | 仕事に貸すメモリ |
| `--cluster-config` | FILE | なし | クラスタの設定（JSON、[3.4](#34-クラスタの設定)）。無ければ単一ノード |
| `--egress-allow` | CIDR | なし | 外へつなぐ（pull 入力・push 出力・字幕・PaaS・HLS pull）宛先として許す私設アドレスの範囲。何度でも書ける。[12. セキュリティ](12-security.md) |

### その他

| オプション | 値 | 既定 | 意味 |
|---|---|---|---|
| `--config` | FILE | なし | 設定の JSON（[3.3](#33-設定ファイルjson)） |
| `--data-dir` | DIR | `out`（カレントディレクトリからの相対） | データ置き場（[3.6](#36-データ置き場)）。必ず指定する |
| `--log-json` | — | 無効 | ノードのログを 1 行 1 JSON で標準出力へ（[13. 運用](13-operations.md)） |

### 起動時の検査

次のときは起動しない（終了コード 2）。

- `--jwks` があるのに `--jwt-issuer` か `--jwt-audience` が無い。
- `--https`・`--rtmps` があるのに `--tls-cert` か `--tls-key` が無い。
- `--record-queue` が 16〜1048576 の外。`--max-playback-ttl` が 0。
- `--egress-allow` が CIDR として読めない。
- クラスタの設定の `node_id` が 0、`peers` に自分が無い、`peers` が 1〜7 台の外。

ファイルが読めない・待ち受けを開けない・クラスタの合言葉が 16 文字未満などは、起動の途中で `fatal: ...` として
終了コード 1 で終わる。

## 3.3 設定ファイル（JSON）

`--config FILE` の JSON は、次のキーを持つオブジェクト。どれも省略でき、省略したものは既定値。
**知らないキーはエラー。** コマンドラインのオプションとの対応:

| JSON のキー | オプション | 型 |
|---|---|---|
| `data_dir` | `--data-dir` | 文字列 |
| `node_name` | `--node-name` | 文字列 |
| `http_listen` | `--http` | 文字列（ADDR） |
| `https_listen`・`rtmp_listen`・`rtmps_listen`・`srt_listen`・`rtsp_listen`・`webrtc_udp` | `--https` など | 文字列（ADDR）か `null`（無効） |
| `tls_cert`・`tls_key` | `--tls-cert`・`--tls-key` | 文字列 |
| `public` | `--public-*-url` | `{"rtmp": 文字列, "http": 文字列か null, "srt": 文字列か null, "rtsp": 文字列か null}` |
| `max_playback_ttl_secs` | `--max-playback-ttl` | 数 |
| `reconnect_grace_ms` | `--reconnect-grace` | 数 |
| `record_queue` | `--record-queue` | 数 |
| `retention_sweep_secs` | `--retention-sweep` | 数 |
| `jwks`・`jwt_issuer`・`jwt_audience`・`jwt_tenant_claim` | `--jwks` など | 文字列 |
| `codec_workers` | `--codec-workers` | 数 |
| `codec_worker_bin`・`codec_lib_dir` | 同名 | 文字列 |
| `zoneinfo_dir` | `--zoneinfo` | 文字列 |
| `console_dir`・`sender_dir` | 同名 | 文字列 |
| `capture_synthetic`・`verify_recordings`・`log_json` | 同名 | 真偽 |
| `cluster` | `--cluster-config` | オブジェクト（[3.4](#34-クラスタの設定)） |
| `egress_allow` | `--egress-allow` | 文字列の配列 |
| `net_mbps` | `--net-mbps` | 数 |
| `labels` | `--label` | 文字列 → 文字列のオブジェクト |
| `capacity_cpu_millis`・`capacity_memory_mb` | 同名 | 数か `null` |

注意:

- JSON では `rtmp_listen` を省略すると既定（`0.0.0.0:1935`）になる。無効にするなら `null` を書く。
- `public` の中で省略したキーは既定値（`rtmp`・`http` は [3.2](#公開-url) の既定、`srt`・`rtsp` は無し）。
  `"http": null` と明示すると、再生 URL の `hls` が `/s/...` から始まる相対の形になり、`ingest.whip_url` は出ない。
- `labels` も `--label` と同じ規則で検査します（合わない場合は起動せず、終了コード 2 で終わります）。

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

## 3.4 クラスタの設定

`--cluster-config FILE`（または `--config` の `cluster`）。知らないキーはエラー。使い方は
[9. クラスタとグリッド](09-cluster-grid.md)。

### ClusterConfig

| キー | 必須 | 型 | 意味 |
|---|---|---|---|
| `node_id` | はい | 数（1 以上） | このノードの Raft の番号。`peers` の中に同じ `id` が要る |
| `listen` | はい | ADDR | ノード間の通信（Raft）の待ち受け |
| `peers` | はい | 配列（1〜7） | 投票するノード（自分を含む） |
| `secret_file` | はい | パス | クラスタの合言葉のファイル。全ノードで同じ内容。前後の空白を除いて 16 文字以上 |
| `tls_ca` | いいえ | パス | ノード間の mTLS の認証局（PEM） |
| `tls_cert` | いいえ | パス | このノードの証明書（PEM、SAN に `DNS:senca-cluster`） |
| `tls_key` | いいえ | パス | 上の秘密鍵（PEM） |
| `relay_listen` | いいえ | ADDR | ノード間の中継の待ち受け。既定は `listen` のポート + 1 |

`tls_ca`・`tls_cert`・`tls_key` は 3 つそろえるか、3 つとも書かない。書かなければノード間は平文
（[12. セキュリティ](12-security.md#127-クラスタの-mtls)）。

### ClusterPeer（`peers[]`）

| キー | 必須 | 型 | 意味 |
|---|---|---|---|
| `id` | はい | 数 | そのノードの Raft の番号 |
| `addr` | はい | ADDR | そのノードの Raft の宛先 |
| `api` | いいえ | URL | そのノードの公開 HTTP の URL（例 `http://10.0.0.2:8080`）。視聴者の 307 の案内と、部屋の別ノードへの案内に使う |
| `relay` | いいえ | ADDR | そのノードの中継の宛先。既定は `addr` のポート + 1 |

中継のポートの既定は「Raft のポート + 1」なので、その番号を空けておく。1 台の機械で複数のノードを動かすとき（試験）は、
`relay_listen` と `peers[].relay` を明示してください。いずれかのノードの中継アドレスが、別のノードの Raft または中継のアドレスと
重なる設定（例: 同じホストで Raft を 7000・7001 にし、中継を既定のままにする）は、起動時の検査でエラーになります
（終了コード 2。重なったアドレスをメッセージに示します）。

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

`senca-node` 自身が読む環境変数は無い。設定はすべてオプションと JSON で渡す。
次の環境変数は、ノードが起動する worker が読む。

| 変数 | 読むもの | 意味 |
|---|---|---|
| `SENCA_CODEC_LIB_DIR` | codec worker | コーデックのライブラリを最初に探すディレクトリ（`:` 区切り）。`--codec-lib-dir` があればそちらで置き換わる |
| `SENCA_CODEC_DISABLE` | codec worker | 使わない提供者（`,` 区切り）。`hardware`・`software`、または `nvidia`・`intel-vpl`・`amd-amf`・`vaapi`・`dav1d`・`libvpx`・`svt-av1`・`opus` |
| `LD_LIBRARY_PATH` | codec worker | 名前だけで `dlopen` するときの OS の検索に使われる |
| `SENCA_CAPTURE_SYNTHETIC` | capture worker | `1`・`true` で試験用の機器を出す |
| `SENCA_CAPTURE_DISABLE` | capture worker | 検出しないバックエンド（`,` 区切り: `v4l2`・`alsa`・`decklink`） |
| `SENCA_CAPTURE_LIB_DIR` | capture worker | DeckLink・ALSA のライブラリを探すディレクトリ（`:` 区切り） |

- codec worker は環境を空にして起動し、上の 3 つ（`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH`）
  だけを親から写す。
- capture worker は親の環境を受け継ぐ。
- `compose.yaml` の `SENCA_PUBLIC_HTTP`・`SENCA_PUBLIC_RTMP` は compose の変数の置き換えで、`--public-*-url` に入る。
  ノードは読まない。

## 3.6 データ置き場

`--data-dir` の中身。ノードは必要なものを作る。

| パス | 中身 |
|---|---|
| `node-id` | このノードの ID（128 bit、16 進）。初回に作り、以後は変えない |
| `audit.log` | 監査ログ（1 行 1 JSON、追記） |
| `control/log` | 単一ノードの制御状態の追記ログ（`長さ:u32be | crc32:u32be | JSON`、1 件ごとに fsync） |
| `control/snapshot.json` | 単一ノードの制御状態の写し。ログが 1000 件を超えると作り直し、ログを空にする |
| `control/raft/` | クラスタの Raft の保存（`hardstate`・`snap-*.snap`・`log/*.seg`）。クラスタでは `control/log` は使わない |
| `control/secret-key` | 秘密値を封じる鍵（32 バイト、0600）。無ければ初回に作る。クラスタでは、いずれか 1 台の鍵がクラスタ共通の鍵になり、クラスタシークレットで暗号化して全ノードへ複製されます |
| `control/bootstrap-admin-token` | 最初の管理者トークンの値（0600）。最初の起動でだけ作る |
| `recordings/{録画 ID}/index.jsonl` | 録画の索引（追記のみの JSON Lines: `begin`・`init`・`segment`・`gap`・`end`） |
| `recordings/{録画 ID}/thumbnail.jpg`・`thumbnail.json` | 録画のサムネイルとその情報 |
| `recordings/.trash-{録画 ID}` | 削除の途中（一時的） |
| `segments/{stream ID}/{run}/{連番}.ts`・`.m4s`（と fMP4 の初期化セグメント） | 録画のセグメント（不変）。`run` は stream タスクごとの ID |
| `segments/{stream ID}/orphans/{run}/` | 起動時の回収で、どの索引からも参照されていないと分かったセグメント（消さずに隔離） |
| `snapshots/{stream ID}/{ID}.jpg`・`{ID}.json` | 定期の静止画とその情報 |

- 単一ノードの制御状態（`control/log`・`snapshot.json`）には、再生 URL の署名鍵が平文で入っている。
  データ置き場の権限は [12. セキュリティ](12-security.md) を見て絞る。
- 何をバックアップするかは [13. 運用](13-operations.md)。
