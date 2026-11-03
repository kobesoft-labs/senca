# 2. クイックスタート

ソースからビルドして単一ノードを動かし、RTMP で送って HLS で見るまで。最後に Docker での起動を書く。

- 関係するページ: [1. 概要](01-overview.md)、[3. 設定](03-configuration.md)、[4. 取り込みと再生](04-ingest-playback.md)、[12. セキュリティ](12-security.md)、[14. コーデックと特許](14-codecs-and-patents.md)
- 設計の理由: [ADR-0008 コンテナでの導入](../../dev/adr/0008-container-deployment.md)

このページのコマンドは、Linux x86_64（Ubuntu 26.04）で実際に流して確かめた。

## 2.1 必要なもの

- Linux（codec worker のサンドボックスは Linux の seccomp・Landlock を使う）
- Rust 1.90 以上（`Cargo.toml` の `rust-version`）と C のリンカー
- 試験の送り手・再生に FFmpeg（任意。OBS や他のプレイヤーでもよい）
- ロイヤリティフリーのコーデックのライブラリ（任意。[2.3](#23-コーデックのライブラリ)）

## 2.2 ビルド

```sh
cargo build --release -p senca-node -p senca-codec-worker
```

`target/release/` に `senca-node` と `senca-codec-worker` ができる。`senca-node` は自分と同じディレクトリの
`senca-codec-worker` を起動するので、2 つは同じ場所に置く（別の場所なら `--codec-worker-bin`）。
キャプチャ機器を使うなら `-p senca-capture` も足す（`senca-capture-worker` ができる）。

## 2.3 コーデックのライブラリ

AV1・VP8/VP9・Opus の符号化・復号には次の共有ライブラリを使う（codec worker が実行時に `dlopen` する）。
無くてもノードは動き、受けた映像・音声をそのまま配る・録ることはできる。変換・静止画・サムネイルは 503 になる。

| ライブラリ（soname） | 用途 | Ubuntu 26.04 のパッケージ |
|---|---|---|
| `libdav1d.so.7` | AV1 の復号 | `libdav1d7` |
| `libSvtAv1Enc.so.2` | AV1 の符号化 | `libsvtav1enc2` |
| `libvpx.so.12` | VP8・VP9 の符号化・復号（libvpx 1.16 の ABI に限る） | `libvpx12` |
| `libopus.so.0` | Opus の符号化・復号 | `libopus0` |

探す順番:

1. `--codec-lib-dir DIR`（無ければ環境変数 `SENCA_CODEC_LIB_DIR`）のディレクトリに、その名前のファイルがあればそれを読む。
   `--codec-lib-dir` は `SENCA_CODEC_LIB_DIR` を置き換えて worker に渡す。値は `:` 区切りで複数書ける。
2. 見つからなければ、名前だけで `dlopen` する（OS の標準の検索: `ld.so.cache`・`LD_LIBRARY_PATH`）。

ディストリビューションのパッケージを入れたなら `--codec-lib-dir` は要らない。別の場所に置いたライブラリを
使うときに指定する。読み込めたものは `GET /api/v1/capabilities` の `codecs.transcode` に出る。

H.264/H.265 のソフトウェアのコーデックは同梱せず、読み込みもしない。H.264/H.265 の変換には GPU と
ベンダーのドライバーが要る（[14. コーデックと特許](14-codecs-and-patents.md)）。

## 2.4 ノードを起動する

```sh
./target/release/senca-node --data-dir ./data \
  --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --console-dir ./apps/console
```

- 既定の待ち受けは `0.0.0.0:8080`（HTTP）と `0.0.0.0:1935`（RTMP）。ここでは手元だけに絞っている。
- **`--data-dir` を必ず指定する。** 既定はカレントディレクトリの `out`。
- 応答や再生 URL に入る公開 URL は、既定で `http://127.0.0.1:8080` と `rtmp://127.0.0.1:1935`。
  ポートや外から見えるホスト名が違うなら `--public-http-url`・`--public-rtmp-url` を合わせる（[3. 設定](03-configuration.md)）。

起動すると次のようなログが標準エラーに出る（codec worker のサンドボックスの行は省略）。

```
created the first admin token in ./data/control/bootstrap-admin-token (mode 0600)
2026-11-03T01:10:04.936Z senca-node 0.0.0 started (node f1be5f27675d35f8dd72b88676be8754)
2026-11-03T01:10:04.936Z http listening on 127.0.0.1:8080
2026-11-03T01:10:04.936Z rtmp listening on 127.0.0.1:1935
2026-11-03T01:10:04.942Z grid: registered (cpu 7200 m, mem 20422 MB, gpus 0, capture 0)
2026-11-03T01:10:04.942Z grid: became the scheduler (leader)
```

確かめる:

```sh
curl -s http://127.0.0.1:8080/healthz   # ok
curl -s http://127.0.0.1:8080/readyz    # ready
```

## 2.5 管理者トークン

初回の起動で、最初の管理者トークン（権限 `tenant:admin`・`node:admin`、テナント `default`、期限なし）が作られ、
`data_dir/control/bootstrap-admin-token`（0600）に書かれる。既定のパスワードは無い。

```sh
TOKEN=$(cat ./data/control/bootstrap-admin-token)
```

このトークンは強い。用途ごとに権限を絞ったトークンを作り、ファイルは安全な場所へ移すか消す
（[12. セキュリティ](12-security.md)）。

## 2.6 stream を作る

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

**`stream_key` はこの応答（と鍵の作り直し `POST /api/v1/streams/{id}/keys`）でしか返らない。** 控えておく。
サーバーにはハッシュだけが残る。

```sh
SID=60dd5cca4fa2785a5d8eefc3b1f5d1bc
KEY=sk_zgo54TfcU8PEPmrJDH34OHnjYR5Ot6nv__hlyLSf4pk
```

## 2.7 送る（RTMP）

### FFmpeg（試験用の映像と音声）

```sh
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -tune zerolatency -g 60 -pix_fmt yuv420p \
  -c:a aac -b:a 128k \
  -f flv "rtmp://127.0.0.1:1935/live/$KEY"
```

URL は `rtmp_url` と `stream_key` をつないだもの。RTMP の app 名は `live`（`live` で始まる名前も可）。
キーフレームの間隔は HLS のセグメントの長さ（既定 2 秒）以下にする（上の例は 30 fps で `-g 60` = 2 秒）。

### OBS

設定 → 配信:

- サービス: カスタム
- サーバー: `rtmp://127.0.0.1:1935/live`
- ストリームキー: `sk_...`（作成時の `stream_key`）

出力の設定で、キーフレームの間隔を 2 秒（またはそれ以下）にする。

送り始めると `GET /api/v1/streams/{id}` の `observed_state` が `live` になり、`live` に受けたトラックが出る。

```sh
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$SID
```

## 2.8 再生 URL をもらって見る

ライブの再生には署名付きの URL が要る（権限 `subscribe`）。

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

- 本文を省くと有効期間は 300 秒。`{"ttl_secs": 3600}` のように指定できる（1〜`--max-playback-ttl`、既定の上限 86400）。
- 期限を過ぎた URL・署名の違う URL は 403。
- `whep` は `--webrtc-udp` を指定したノードでだけ使える（[4. 取り込みと再生](04-ingest-playback.md)）。

見る:

```sh
ffplay "http://127.0.0.1:8080/s/.../index.m3u8"
# または
ffprobe -v error -show_entries stream=codec_name,width,height -of compact "http://127.0.0.1:8080/s/.../index.m3u8"
```

ブラウザーでは hls.js などの HLS プレイヤーで再生できる（署名付きの URL の応答は `Access-Control-Allow-Origin: *`）。
Safari・iOS での再生はまだ確かめていない（[1.5 状態](01-overview.md#15-状態何を確かめたか)）。
管理画面は `http://127.0.0.1:8080/console/`（API トークンでログインする。管理画面の再生器は fMP4 の HLS か WHEP なので、
既定の TS の stream はそこでは再生できない。`hls.container` は [4. 取り込みと再生](04-ingest-playback.md)）。

## 2.9 録画してみる（任意）

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:8080/api/v1/streams/$SID/recordings          # 202、state は recording
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/recordings
```

止める・VOD で見る・MP4 に書き出すのは [5. 録画](05-recording.md)。

## 2.10 止める

`Ctrl-C`（SIGINT）か SIGTERM。録画中のものは `finalized` で閉じてから終わる。
強制終了（SIGKILL・停電）の後は、次の起動で書きかけの録画が `interrupted` として閉じられる（[13. 運用](13-operations.md)）。

## 2.11 Docker

リポジトリの `Dockerfile` と `compose.yaml` で単一ノードを動かせる。

```sh
docker compose up -d --build
docker compose exec senca cat /var/lib/senca/control/bootstrap-admin-token
```

- 管理 API・HLS・管理画面: `http://localhost:8080`（管理画面は `/console/`）
- RTMP: `rtmp://localhost:1935/live`、SRT: `localhost:9000/udp`
- 公開 URL は compose の変数 `SENCA_PUBLIC_HTTP`（既定 `http://localhost:8080`）・`SENCA_PUBLIC_RTMP`
  （既定 `rtmp://localhost:1935`）で変える。例: `SENCA_PUBLIC_HTTP=https://media.example.com docker compose up -d`

イメージの中身:

| 項目 | 値 |
|---|---|
| ベース | Ubuntu 26.04（libvpx 1.16 = `libvpx.so.12` を持つため） |
| 実行ファイル | `/opt/senca/bin/senca-node`・`senca-codec-worker`・`senca-capture-worker` |
| コーデックのライブラリ | `libdav1d7`・`libvpx12`・`libopus0`・`libsvtav1enc2`（と GPU 用の `libva2`・`libva-drm2`・`libvpl2`）。`--codec-lib-dir /usr/lib/x86_64-linux-gnu` |
| 利用者 | `senca`（uid 10001）。compose では読み取り専用のルート・`/tmp` は tmpfs・`no-new-privileges` |
| データ | ボリューム `/var/lib/senca`（`--data-dir`） |
| 管理画面・送出ページ | `/opt/senca/console`（`/console/`）・`/opt/senca/sender`（`/sender/`） |
| ヘルスチェック | `senca-node --healthcheck http://127.0.0.1:8080/readyz`（10 秒ごと） |
| 公開するポート | 8080/tcp・1935/tcp・9000/udp・8443/udp |

注意:

- イメージの `ENTRYPOINT` は `--data-dir`・`--console-dir`・`--sender-dir`・`--codec-lib-dir` を固定し、
  `CMD`（compose の `command`）で待ち受けを渡す。`command` を書き換えるときは `--http` などを全部書く。
- WebRTC（WHIP・WHEP・部屋）は既定の `command` では有効になっていない（8443/udp を公開しているだけ）。
  使うなら `command` に `--webrtc-udp <ホストの IP>:8443` を足し、`ports` に `"8443:8443/udp"` を足す。
  `--webrtc-udp` のアドレスはそのまま ICE の候補として相手に渡るので、`0.0.0.0` ではなく相手から届く IP を書く。
- `compose.yaml` は作業ツリーからビルドする。配布するイメージはコミット済みのソースから作る
  （`git archive HEAD | docker build -t senca/node:local -`、ADR-0008）。
- GPU を渡す例は `compose.yaml` のコメントにある（NVIDIA は NVIDIA Container Toolkit、Intel/AMD は `/dev/dri`）。
- イメージの中のライブラリの置き場所は x86_64 の名前（`/usr/lib/x86_64-linux-gnu`）で固定している。

## 2.12 次に読むもの

- 受信・再生の詳細（SRT・WHIP・LL-HLS・fMP4）: [4. 取り込みと再生](04-ingest-playback.md)
- すべてのオプション: [3. 設定](03-configuration.md)
- 公開する前に: [12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
