# 16. コンテナイメージ

[English](../en/16-containers.md)

SENCA のノードのコンテナイメージを、GitHub Container Registry（`ghcr.io/kobesoft-labs/senca`）で公開しています。
用途に合わせて 3 つの種類があり、amd64（一般的なサーバー）と arm64（Raspberry Pi 4・5 など）の両方で使えます。
GPU のドライバーや独自のライブラリは、あとから自分で足せます。

## 16.1 種類とタグ

| 種類 | タグ | アーキテクチャー | 中身 |
|---|---|---|---|
| standard | `latest`・`<版>`（例: `0.0.2`） | amd64・arm64 | slim ＋ ロイヤリティフリーのコーデック（dav1d・libvpx・SVT-AV1・libopus）、VA-API の実行時ライブラリ（amd64 は oneVPL の dispatcher も） |
| slim | `slim`・`<版>-slim` | amd64・arm64 | ノード本体だけ。コーデックのライブラリは入っていません |
| gpu | `gpu`・`<版>-gpu` | amd64 | standard ＋ GPU のドライバー（Intel の VA-API と oneVPL の GPU 実行時、AMD の VA-API） |

- `main` ブランチの最新は `edge`・`edge-slim`・`edge-gpu` です。
- `docker pull` は、使っている機械のアーキテクチャーに合うものを自動で選びます。

種類の選び方:

- **受け取った映像をそのまま配信・録画・中継するだけ**なら slim で足ります。H.264・H.265・AAC は変換せずにそのまま運べます
  （[4. 取り込みと再生](04-ingest-playback.md)）。いちばん小さく、Raspberry Pi に向いています。
- **AV1・VP8・VP9・Opus の変換**や、パイプライン（[7. パイプライン](07-pipelines.md)）を CPU で動かすなら standard を使います。
- **H.264・H.265 の変換**には GPU が要ります（[14. コーデックと特許](14-codecs-and-patents.md)）。Intel・AMD の GPU は gpu を、
  NVIDIA の GPU はどの種類でも使えます（[16.4](#164-gpu-を使う)）。

## 16.2 起動する

```sh
docker run -d --name senca \
  -p 8080:8080 -p 1935:1935 -p 9000:9000/udp -p 8443:8443/udp \
  -v senca-data:/var/lib/senca \
  ghcr.io/kobesoft-labs/senca:latest \
  --http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 --webrtc-udp 0.0.0.0:8443 \
  --webrtc-public-ip 203.0.113.10 \
  --public-http-url http://203.0.113.10:8080 --public-rtmp-url rtmp://203.0.113.10:1935
docker exec senca cat /var/lib/senca/control/bootstrap-admin-token
```

- イメージ名の後ろに書いた引数は、ノードのオプション（[3. 設定](03-configuration.md)）です。何も書かなければ、
  `--http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 --webrtc-udp 0.0.0.0:8443` で起動します。
  引数を書く場合は、待ち受けのオプションもすべて書いてください。
- `--data-dir /var/lib/senca`・`--console-dir`・`--sender-dir` はイメージで指定済みです。
- `203.0.113.10` は、クライアントから見えるホストの IP アドレスに置き換えてください。WebRTC（WHIP・WHEP・部屋）は
  `--webrtc-public-ip` が無いとホストの外からつながりません。

`compose.yaml` を使う場合は、リポジトリで次のようにします。種類は `SENCA_IMAGE` で選べます。

```sh
SENCA_PUBLIC_IP=203.0.113.10 docker compose up -d
SENCA_IMAGE=ghcr.io/kobesoft-labs/senca:slim docker compose up -d
```

## 16.3 Raspberry Pi

64 bit の OS（Raspberry Pi OS の 64 bit 版や Ubuntu の arm64 版）の Raspberry Pi 4・5 で、`linux/arm64` のイメージが動きます。
32 bit の OS には対応していません。

```sh
docker run -d --name senca --restart unless-stopped \
  -p 8080:8080 -p 1935:1935 -p 9000:9000/udp \
  -v senca-data:/var/lib/senca \
  --device /dev/video0 --group-add "$(getent group video | cut -d: -f3)" \
  ghcr.io/kobesoft-labs/senca:slim \
  --http 0.0.0.0:8080 --rtmp 0.0.0.0:1935 --srt 0.0.0.0:9000 \
  --public-http-url http://raspberrypi.local:8080 --public-rtmp-url rtmp://raspberrypi.local:1935
```

- Raspberry Pi の GPU（V4L2 の符号化器）には対応していないので、H.264・H.265 の変換はできません。受け取った映像の配信・録画・
  中継（グリッドのノードとしての参加も含む）はそのまま使えます。
- USB カメラ・キャプチャーデバイスは `--device /dev/video0` で渡します（[6. 入力と出力](06-inputs-outputs.md) のキャプチャーデバイス）。
  音声（ALSA）は `--device /dev/snd` で渡します。コンテナーは uid 10001 のユーザーで動くので、デバイスのグループ（例では `video`）を
  `--group-add` で加えてください。
- 録画を SD カードに書き続けると傷みやすいので、USB の SSD などをボリュームに使うか、S3 互換のストレージ（[5. 録画](05-recording.md)）に
  置くのがおすすめです。

## 16.4 GPU を使う

| GPU | 種類 | 渡し方 |
|---|---|---|
| NVIDIA（NVENC・NVDEC） | どれでも | ホストに NVIDIA のドライバーと NVIDIA Container Toolkit を入れ、`--gpus all`。ライブラリは Toolkit がコンテナーに入れます（イメージに `NVIDIA_DRIVER_CAPABILITIES=compute,video,utility` を指定済み） |
| Intel（VA-API・oneVPL） | gpu | `--device /dev/dri --group-add "$(getent group render \| cut -d: -f3)"` |
| AMD（VA-API） | gpu | Intel と同じ |
| AMD（AMF） | gpu | 上に加えて、AMD のドライバーの AMF の実行時ライブラリ（`libamfrt64.so.1`）を `/opt/senca/lib` にマウント（[16.5](#165-ドライバーやライブラリを足す)） |

使えるかどうかは、管理画面の「ノードとデバイス」か `GET /api/v1/devices` で確かめられます。使えない場合は、読み込めなかった
ライブラリの名前と理由が表示されます。

## 16.5 ドライバーやライブラリを足す

方法は 3 つあります。

### 実行時にマウントする

共有ライブラリ（`.so`）を置いたディレクトリを `/opt/senca/lib` にマウントします。codec worker とキャプチャーの worker は、
まずここからライブラリを探し、無ければシステムの既定の場所から探します。ここは `LD_LIBRARY_PATH` にも入っているので、
足したライブラリが必要とするライブラリも同じ場所に置けます。

```sh
docker run ... -v /opt/amdgpu-pro/lib/x86_64-linux-gnu:/opt/senca/lib:ro ghcr.io/kobesoft-labs/senca:gpu
```

DeckLink の `libDeckLinkAPI.so` や、AMD の AMF の実行時など、ホストのドライバーと一緒に入るライブラリに向いています。
読み込むのは、各プロバイダーで決まった名前（soname）のライブラリだけです（[14. コーデックと特許](14-codecs-and-patents.md)）。

### イメージを派生させる

公開イメージを元に、Ubuntu 26.04 のパッケージやファイルを足します。

```dockerfile
FROM ghcr.io/kobesoft-labs/senca:gpu
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends intel-media-va-driver-non-free \
 && rm -rf /var/lib/apt/lists/*
COPY mylibs/ /opt/senca/lib/
USER senca
```

最後に `USER senca` に戻してください（ノードは root では動かさない前提で作っています）。

### ソースからビルドする

リポジトリの `Dockerfile` を使い、`--target` で種類を、`--build-arg` で追加のパッケージを指定します。

```sh
docker build --target slim -t my/senca:slim .
docker build --target standard --build-arg EXTRA_PACKAGES="alsa-utils v4l-utils" -t my/senca .
docker build --target gpu --build-arg INTEL_VA_DRIVER=intel-media-va-driver-non-free -t my/senca:gpu .
docker buildx build --platform linux/arm64 --target slim -t my/senca:slim-arm64 .   # 別のアーキテクチャー（QEMU が要ります）
```

| 引数 | 既定 | 意味 |
|---|---|---|
| `EXTRA_PACKAGES` | なし | どの種類にも入れる Ubuntu 26.04 のパッケージ（空白区切り。main・universe・restricted・multiverse から） |
| `INTEL_VA_DRIVER` | `intel-media-va-driver` | gpu に入れる Intel の VA-API のドライバー。`intel-media-va-driver-non-free` にすると、対応する符号化が増えます |
| `UBUNTU` | `ubuntu:26.04` | ベースのイメージ。libvpx のプロバイダーが `libvpx.so.12` を前提にしているので、26.04 より古いものでは VP8・VP9 が使えません |

## 16.6 イメージの中身

| 項目 | 値 |
|---|---|
| ベースイメージ | Ubuntu 26.04 |
| 実行ファイル | `/opt/senca/bin/senca-node`・`senca-codec-worker`・`senca-capture-worker` |
| 追加のライブラリの置き場所 | `/opt/senca/lib`（空。`SENCA_CODEC_LIB_DIR`・`SENCA_CAPTURE_LIB_DIR`・`LD_LIBRARY_PATH` に指定済み） |
| 実行ユーザー | `senca`（uid 10001） |
| データ | ボリューム `/var/lib/senca` |
| 管理画面・配信ページ | `/opt/senca/console`（`/console/`）・`/opt/senca/sender`（`/sender/`） |
| ヘルスチェック | `senca-node --healthcheck http://127.0.0.1:8080/readyz`（10 秒ごと） |
| 公開するポート | 8080/tcp（HTTP）・1935/tcp（RTMP）・9000/udp（SRT）・8443/udp（WebRTC） |
| 依存ライブラリのライセンス | `/opt/senca/licenses` |

イメージは GitHub Actions（`.github/workflows/docker.yml`）で、amd64 と arm64 をそれぞれのアーキテクチャーの機械で作っています。
`v` で始まるタグを push すると版のタグと `latest` が、`main` に push すると `edge` が更新されます。
