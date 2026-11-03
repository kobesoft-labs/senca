# 2. クイックスタート

このページでは、ソースから SENCA をビルドして単一ノードを起動し、RTMP で配信したストリームを HLS で再生するまでの手順を説明します。
最後に、Docker で起動する方法も紹介します。

- 関連するページ: [1. 概要](01-overview.md)、[3. 設定](03-configuration.md)、[4. 取り込みと再生](04-ingest-playback.md)、[12. セキュリティ](12-security.md)、[14. コーデックと特許](14-codecs-and-patents.md)
- 設計の背景: [ADR-0008 コンテナでの導入](../../dev/adr/0008-container-deployment.md)

このページのコマンドは、Linux x86_64（Ubuntu 26.04）で動作を確認しています。

## 2.1 必要なもの

- Linux（codec worker のサンドボックスに、Linux の seccomp・Landlock を使います）
- Rust 1.90 以上（`Cargo.toml` の `rust-version`）と C のリンカー
- テスト用の配信と再生に使う FFmpeg（任意。OBS や他のプレイヤーでもかまいません）
- ロイヤリティフリーのコーデックのライブラリ（任意。[2.3](#23-コーデックのライブラリ) を参照）

## 2.2 ビルド

```sh
cargo build --release -p senca-node -p senca-codec-worker
```

`target/release/` に `senca-node` と `senca-codec-worker` が作られます。`senca-node` は自身と同じディレクトリにある
`senca-codec-worker` を起動するため、2 つのファイルは同じ場所に置いてください（別の場所に置く場合は `--codec-worker-bin` で指定します）。
キャプチャデバイスを使う場合は、`-p senca-capture` も追加してください（`senca-capture-worker` が作られます）。

## 2.3 コーデックのライブラリ

AV1・VP8/VP9・Opus のエンコード・デコードには、次の共有ライブラリを使います（codec worker が実行時に `dlopen` で読み込みます）。
これらがなくてもノードは動作し、受信した映像・音声をそのまま配信・録画することはできます。変換・静止画・サムネイルは 503 になります。

| ライブラリ（soname） | 用途 | Ubuntu 26.04 のパッケージ |
|---|---|---|
| `libdav1d.so.7` | AV1 のデコード | `libdav1d7` |
| `libSvtAv1Enc.so.2` | AV1 のエンコード | `libsvtav1enc2` |
| `libvpx.so.12` | VP8・VP9 のエンコード・デコード（libvpx 1.16 の ABI のみ） | `libvpx12` |
| `libopus.so.0` | Opus のエンコード・デコード | `libopus0` |

ライブラリは次の順に探します。

1. `--codec-lib-dir DIR`（指定がなければ環境変数 `SENCA_CODEC_LIB_DIR`）のディレクトリに、その名前のファイルがあれば読み込みます。
   `--codec-lib-dir` を指定すると、`SENCA_CODEC_LIB_DIR` を置き換えて worker に渡します。値は `:` 区切りで複数指定できます。
2. 見つからなければ、ファイル名だけで `dlopen` します（OS 標準の検索: `ld.so.cache`・`LD_LIBRARY_PATH`）。

ディストリビューションのパッケージをインストールした場合、`--codec-lib-dir` は不要です。別の場所に置いたライブラリを使う場合に
指定してください。読み込めたコーデックは、`GET /api/v1/capabilities` の `codecs.transcode` に表示されます。

H.264/H.265 のソフトウェアコーデックは同梱しておらず、読み込むこともありません。H.264/H.265 の変換には、GPU とメーカーの
ドライバーが必要です（[14. コーデックと特許](14-codecs-and-patents.md)）。

## 2.4 ノードを起動する

```sh
./target/release/senca-node --data-dir ./data \
  --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --console-dir ./apps/console
```

- 既定の待ち受けアドレスは `0.0.0.0:8080`（HTTP）と `0.0.0.0:1935`（RTMP）です。この例では、ローカルホストからの接続だけを受け付けるようにしています。
- **`--data-dir` は必ず指定してください。** 既定値は、カレントディレクトリの `out` です。
- API の応答や再生 URL に含まれる公開 URL は、既定で `http://127.0.0.1:8080` と `rtmp://127.0.0.1:1935` です。
  ポート番号や外部から見たホスト名が異なる場合は、`--public-http-url`・`--public-rtmp-url` で合わせてください（[3. 設定](03-configuration.md)）。

起動すると、標準エラー出力に次のようなログが出力されます（codec worker のサンドボックスに関する行は省略しています）。

```
created the first admin token in ./data/control/bootstrap-admin-token (mode 0600)
2026-11-03T01:10:04.936Z senca-node 0.0.0 started (node f1be5f27675d35f8dd72b88676be8754)
2026-11-03T01:10:04.936Z http listening on 127.0.0.1:8080
2026-11-03T01:10:04.936Z rtmp listening on 127.0.0.1:1935
2026-11-03T01:10:04.942Z grid: registered (cpu 7200 m, mem 20422 MB, gpus 0, capture 0)
2026-11-03T01:10:04.942Z grid: became the scheduler (leader)
```

起動を確認します。

```sh
curl -s http://127.0.0.1:8080/healthz   # ok
curl -s http://127.0.0.1:8080/readyz    # ready
```

## 2.5 管理者トークン

初回の起動時に、最初の管理者トークン（権限 `tenant:admin`・`node:admin`、テナント `default`、有効期限なし）が作成され、
`data_dir/control/bootstrap-admin-token`（パーミッション 0600）に書き込まれます。既定のパスワードはありません。

```sh
TOKEN=$(cat ./data/control/bootstrap-admin-token)
```

このトークンは強い権限を持っています。用途ごとに権限を絞ったトークンを作成し、このファイルは安全な場所へ移すか削除してください
（[12. セキュリティ](12-security.md)）。

## 2.6 ストリームを作成する

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/streams \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"demo"}'
```

応答（201、抜粋）:

```json
{
  "id": "60dd5cca4fa2785a5d8eefc3b1f5d1bc",
  "name": "demo",
  "generation": 1,
  "desired_state": "running",
  "observed_state": "waiting_input",
  "hls": {"container": "ts", "segment_ms": 2000, "window": 6},
  "ingest": {
    "rtmp_url": "rtmp://127.0.0.1:1935/live",
    "stream_key": "sk_zgo54TfcU8PEPmrJDH34OHnjYR5Ot6nv__hlyLSf4pk",
    "whip_url": "http://127.0.0.1:8080/whip/60dd5cca4fa2785a5d8eefc3b1f5d1bc"
  }
}
```

**`stream_key` が返されるのは、この応答と、キーを再発行したとき（`POST /api/v1/streams/{id}/keys`）だけです。** 必ず控えておいてください。
サーバーにはハッシュ値だけが保存されます。

```sh
SID=60dd5cca4fa2785a5d8eefc3b1f5d1bc
KEY=sk_zgo54TfcU8PEPmrJDH34OHnjYR5Ot6nv__hlyLSf4pk
```

## 2.7 RTMP で配信する

### FFmpeg（テスト用の映像と音声）

```sh
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -tune zerolatency -g 60 -pix_fmt yuv420p \
  -c:a aac -b:a 128k \
  -f flv "rtmp://127.0.0.1:1935/live/$KEY"
```

配信先の URL は、`rtmp_url` の後ろに `stream_key` をつなげたものです。RTMP の app 名は `live` です（`live` で始まる名前も使えます）。
キーフレームの間隔は、HLS のセグメントの長さ（既定 2 秒）以下にしてください（上の例では 30 fps で `-g 60`、つまり 2 秒です）。

### OBS

［設定］→［配信］で次のように設定します。

- サービス: カスタム
- サーバー: `rtmp://127.0.0.1:1935/live`
- ストリームキー: `sk_...`（作成時に返された `stream_key`）

［出力］の設定で、キーフレームの間隔を 2 秒（またはそれ以下）にしてください。

配信を始めると、`GET /api/v1/streams/{id}` の `observed_state` が `live` になり、`live` に受信したトラックの情報が表示されます。

```sh
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$SID
```

## 2.8 再生 URL を取得して再生する

ライブの再生には、署名付きの URL が必要です（権限 `subscribe`）。

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8080/api/v1/streams/$SID/playback-url
```

```json
{
  "expires_at": "2026-11-03T01:16:01Z",
  "playback": {
    "hls": "http://127.0.0.1:8080/s/1793668561/nWqDQOWkMTMY3Z2GoC94Rw/live/60dd5cca4fa2785a5d8eefc3b1f5d1bc/index.m3u8",
    "rtmp": "rtmp://127.0.0.1:1935/play/60dd5cca4fa2785a5d8eefc3b1f5d1bc?exp=1793668561&sig=nWqDQOWkMTMY3Z2GoC94Rw",
    "whep": "http://127.0.0.1:8080/whep/60dd5cca4fa2785a5d8eefc3b1f5d1bc?exp=1793668561&sig=nWqDQOWkMTMY3Z2GoC94Rw"
  }
}
```

- リクエストの本文を省略すると、有効期間は 300 秒です。`{"ttl_secs": 3600}` のように指定できます（1〜`--max-playback-ttl`。上限の既定値は 86400）。
- 有効期限を過ぎた URL や、署名が一致しない URL は 403 になります。
- `whep` は、`--webrtc-udp` を指定したノードでのみ使えます（[4. 取り込みと再生](04-ingest-playback.md)）。

再生します。

```sh
ffplay "http://127.0.0.1:8080/s/.../index.m3u8"
# または
ffprobe -v error -show_entries stream=codec_name,width,height -of compact "http://127.0.0.1:8080/s/.../index.m3u8"
```

ブラウザーでは、hls.js などの HLS プレイヤーで再生できます（署名付き URL の応答には `Access-Control-Allow-Origin: *` が付きます）。
Safari・iOS での再生は、現在確認していません（[1.5 動作確認の状況](01-overview.md#15-動作確認の状況)）。

管理画面は `http://127.0.0.1:8080/console/` です（API トークンでログインします）。管理画面のプレイヤーは fMP4 の HLS か WHEP に
対応しているため、既定の TS のストリームは管理画面では再生できません。`hls.container` については [4. 取り込みと再生](04-ingest-playback.md) を参照してください。

## 2.9 録画する（任意）

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:8080/api/v1/streams/$SID/recordings          # 202、state は recording
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/recordings
```

録画の停止、VOD での再生、MP4 への書き出しについては [5. 録画](05-recording.md) を参照してください。

## 2.10 停止する

`Ctrl-C`（SIGINT）または SIGTERM で停止します。録画中のものは `finalized` として閉じてから終了します。
強制終了（SIGKILL・停電など）した場合は、次の起動時に書き込み途中の録画が `interrupted` として閉じられます（[13. 運用](13-operations.md)）。

## 2.11 Docker

リポジトリの `Dockerfile` と `compose.yaml` を使って、単一ノードを起動できます。

```sh
docker compose up -d --build
docker compose exec senca cat /var/lib/senca/control/bootstrap-admin-token
```

- 管理 API・HLS・管理画面: `http://localhost:8080`（管理画面は `/console/`）
- RTMP: `rtmp://localhost:1935/live`、SRT: `localhost:9000/udp`
- 公開 URL は、compose の変数 `SENCA_PUBLIC_HTTP`（既定 `http://localhost:8080`）・`SENCA_PUBLIC_RTMP`
  （既定 `rtmp://localhost:1935`）で変更できます。例: `SENCA_PUBLIC_HTTP=https://media.example.com docker compose up -d`

イメージの内容は次のとおりです。

| 項目 | 値 |
|---|---|
| ベースイメージ | Ubuntu 26.04（libvpx 1.16 = `libvpx.so.12` を含むため） |
| 実行ファイル | `/opt/senca/bin/senca-node`・`senca-codec-worker`・`senca-capture-worker` |
| コーデックのライブラリ | `libdav1d7`・`libvpx12`・`libopus0`・`libsvtav1enc2`（と GPU 用の `libva2`・`libva-drm2`・`libvpl2`）。`--codec-lib-dir /usr/lib/x86_64-linux-gnu` |
| 実行ユーザー | `senca`（uid 10001）。compose では、ルートファイルシステムを読み取り専用にし、`/tmp` を tmpfs にし、`no-new-privileges` を指定しています |
| データ | ボリューム `/var/lib/senca`（`--data-dir`） |
| 管理画面・配信ページ | `/opt/senca/console`（`/console/`）・`/opt/senca/sender`（`/sender/`） |
| ヘルスチェック | `senca-node --healthcheck http://127.0.0.1:8080/readyz`（10 秒ごと） |
| 公開するポート | 8080/tcp・1935/tcp・9000/udp・8443/udp |

注意点:

- イメージの `ENTRYPOINT` は `--data-dir`・`--console-dir`・`--sender-dir`・`--codec-lib-dir` を固定しており、待ち受けアドレスは
  `CMD`（compose の `command`）で渡します。`command` を書き換える場合は、`--http` などをすべて書いてください。
- WebRTC（WHIP・WHEP・部屋）は、既定の `command` では有効になっていません（8443/udp を公開しているだけです）。
  使う場合は、`command` に `--webrtc-udp <ホストの IP>:8443` を、`ports` に `"8443:8443/udp"` を追加してください。
  `--webrtc-udp` のアドレスはそのまま ICE の候補として相手に伝わるため、`0.0.0.0` ではなく、相手から到達できる IP アドレスを指定してください。
- `compose.yaml` は作業ツリーからビルドします。配布するイメージは、コミット済みのソースから作成してください
  （`git archive HEAD | docker build -t senca/node:local -`、ADR-0008）。
- GPU をコンテナに渡す例は、`compose.yaml` のコメントにあります（NVIDIA は NVIDIA Container Toolkit、Intel/AMD は `/dev/dri`）。
- イメージ内のライブラリの配置先は、x86_64 のパス（`/usr/lib/x86_64-linux-gnu`）に固定しています。

## 2.12 次に読むページ

- 取り込み・再生の詳細（SRT・WHIP・LL-HLS・fMP4）: [4. 取り込みと再生](04-ingest-playback.md)
- すべてのオプション: [3. 設定](03-configuration.md)
- 外部に公開する前に: [12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
