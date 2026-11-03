# 14. コーデックと特許

[English](../en/14-codecs-and-patents.md)

この章では、SENCA がどのコーデックをどのように扱うかを説明します。
製品に含めないもの、実行時に読み込むもの、運用者が用意するもの（GPU のドライバー、ロイヤリティフリーのコーデックライブラリ）と、
それらが無いときの動作をまとめます。

- 設計の背景: [ADR-0004 コーデックの選定と特許の方針](../../dev/adr/0004-codecs-patents.md)、[ADR-0003 内部のメディア形式](../../dev/adr/0003-internal-media-format.md)
- 台帳: [ライセンス・特許の台帳](../../licenses/README.md)
- 関連するページ: [4. 取り込みと再生](04-ingest-playback.md)、[5. 録画](05-recording.md)、[7. パイプライン](07-pipelines.md)、[11. メディアパイプライン](11-media-pipelines.md)、[13. 運用](13-operations.md)

> **法的な助言ではありません。** この章と台帳は、SENCA の実装の方針を説明するものです。配信するコンテンツ・サービス・地域で
> どのライセンス（特許プールの料金を含む）が必要かは、利用者ご自身で確認してください。

## 14.1 方針

1. **H.264・H.265 のエンコード・デコードは、GPU ベンダーのハードウェアコーデックだけで行います。** ソフトウェアの H.264/H.265 コーデック
   （libavcodec、OpenH264、x264、x265 など）は製品に含めず、読み込みもしません。
2. **AAC はそのまま通す（パススルー）だけです。** AAC のエンコード・デコード（fdk-aac など）は含めません。音声を変換するときは Opus にします。
3. **ロイヤリティフリーとして提供されているコーデックは、ソフトウェアで使います**: dav1d（AV1 のデコード）、SVT-AV1（AV1 のエンコード）、
   libvpx（VP8/VP9）、libopus（Opus）。
4. 外部のコーデックライブラリは製品に同梱せず、**別プロセスの `senca-codec-worker` が実行時に読み込みます（dlopen）**。
   ノード本体はコーデックライブラリを読み込みません。
5. 必要なデバイスやライブラリが無いときは、黙って品質を落とすことはせず、理由を付けてエラーを返します（503 `CAPACITY_UNAVAILABLE` や検証のエラー）。

H.264/H.265 の**ビットストリームの解析と形式変換**（Annex B ⇔ 長さプレフィックス、SPS/PPS/VPS の解析、RTP のパケット化、TS・fMP4・FLV への格納）は
エンコード・デコードではないため、SENCA 自身が行います。

## 14.2 パススルーはいつでも使える

エンコード・デコードをしない経路では、GPU もコーデックライブラリも必要ありません。

| 経路 | パススルーできるコーデック |
|---|---|
| 取り込み（RTMP・SRT・WHIP・pull） | H.264・H.265・AV1・VP8・VP9・AAC・Opus・G.711（経路ごとの対応は [4. 取り込みと再生](04-ingest-playback.md)） |
| HLS（TS） | H.264・H.265・AAC・Opus |
| HLS（fMP4） | H.264・H.265・AV1・VP9・AAC・Opus |
| 録画・VOD・MP4 の書き出し | HLS と同じ（同じセグメントを使います） |
| RTMP play・`rtmp_push` | FLV・Enhanced RTMP で運べるもの |
| SRT play・`srt_push` | TS で運べるもの |
| RTSP play | RTP にできるもの |
| WHEP | H.264・VP8・VP9・AV1・Opus・PCMA・PCMU（**AAC は運べない**ため、AAC の音声は出ません） |
| 字幕 | AAC・Opus の音声を詰め替えて送ります（[8. 字幕](08-captions.md)） |

`POST /streams/{id}/plan` は、現在の入力の各トラックが各出力でどう扱われるか（`remux`・`repacketize`・`passthrough`・`unsupported` と理由）を返します。

## 14.3 エンコード・デコードが必要な操作

| 操作 | 必要なもの | 無いとき |
|---|---|---|
| 現在の画像 `GET /streams/{id}/snapshot` | 映像のデコード | 503 `CAPACITY_UNAVAILABLE`（`retryable: false`） |
| 静止画の保存（`snapshots` の設定・`snapshot` の予約） | 映像のデコード | 保存されません（ログに理由） |
| 録画のサムネイル | 映像のデコード | 作成されません（`GET /recordings/{id}/thumbnail` は 404） |
| パイプライン（`decode`・`encode`） | 処理ノードごとのデコード・エンコード | 作成・変更が 503 `CAPACITY_UNAVAILABLE`、または 400 `POLICY_REJECTED`・`UNSUPPORTED_CODEC`（[7. パイプライン](07-pipelines.md)） |
| 部屋のメディアパイプラインの合成 | 参加者の映像のデコードと合成のエンコード（既定 AV1） | [11. メディアパイプライン](11-media-pipelines.md) を参照 |
| キャプチャの入力 | 取り込み用 worker でのエンコード（既定 VP9） | 入力が `error`・`backoff` になります |

例: GPU の無いノードで、H.264 のストリームの現在の画像を要求したときの応答:

```json
{"type":"urn:senca:error:capacity-unavailable","title":"h264 を復号できる装置がありません（H.264/H.265 はハードウェアのデコーダーだけを使います）",
 "status":503,"code":"CAPACITY_UNAVAILABLE","retryable":false}
```

VP9 のストリームなら、同じノードでも libvpx でデコードでき、JPEG・PNG が返ります。

## 14.4 ハードウェアのプロバイダー

| プロバイダー（`provider`） | API | 読み込むライブラリ | エンコード | デコード |
|---|---|---|---|---|
| `nvidia` | NVENC・NVDEC（Video Codec SDK 13.1、Linux ドライバー 610 以上） | `libcuda.so.1`・`libnvidia-encode.so.1`・`libnvcuvid.so.1`（`libnvidia-ml.so.1` はバージョン表示だけに使い、無くてもかまいません） | H.264・H.265・AV1 | H.264・H.265・AV1・VP9 |
| `intel-vpl` | oneVPL（Quick Sync） | `libvpl.so.2`（dispatcher がドライバーの実装を探します） | H.264・H.265・AV1 | H.264・H.265・AV1・VP9 |
| `amd-amf` | AMF（Linux では Vulkan） | `libamfrt64.so.1` | H.264・H.265・AV1 | H.264・H.265 |
| `vaapi` | VA-API | `libva.so.2`・`libva-drm.so.2`、`/dev/dri/renderD*` | なし | H.264 |

- どのプロバイダーも、ハードウェアの実装だけを使います（oneVPL・AMF のソフトウェア実装は使いません）。
- ライブラリやデバイスが無いことは正常な状態として扱い、そのプロバイダーを「使えない」とします（`GET /devices` に理由が表示されます）。
- 使う前に、お使いの GPU とドライバーで動作を確認してください。
- Apple VideoToolbox には対応していません。

コンテナで GPU を使うときは、デバイスとドライバーのライブラリをコンテナに渡します（[13. 運用](13-operations.md)、`compose.yaml`）。

## 14.5 ロイヤリティフリーのソフトウェアコーデック

| プロバイダー | ライブラリ（soname） | 用途 | ライセンス |
|---|---|---|---|
| `dav1d` | `libdav1d.so.7` | AV1 のデコード | BSD-2-Clause |
| `svt-av1` | `libSvtAv1Enc.so.2` | AV1 のエンコード | BSD-3-Clause＋AOM の特許許諾 |
| `libvpx` | `libvpx.so.12`（libvpx 1.16 の ABI） | VP8・VP9 のエンコード・デコード | BSD-3-Clause＋WebM の特許許諾 |
| `opus` | `libopus.so.0` | Opus のエンコード・デコード | BSD-3-Clause＋IETF での宣言に基づく特許許諾 |

### インストール

ディストリビューションのパッケージをインストールします。公式のコンテナイメージ（`Dockerfile`）は Ubuntu 26.04 で、standard と gpu には次のパッケージを入れています（`libvpl2` は amd64 だけ）。

```sh
apt-get install -y --no-install-recommends libdav1d7 libvpx12 libopus0 libsvtav1enc2 libva2 libva-drm2 libvpl2
```

soname が合わないバージョン（例: `libvpx.so.9` しか無い）は読み込みません。libvpx のプロバイダーは libvpx 1.16（`libvpx.so.12`）の ABI に合わせています。

ライブラリを探す場所:

1. `--codec-lib-dir DIR`（ノードが worker の環境変数 `SENCA_CODEC_LIB_DIR` に渡します。`:` 区切りで複数指定できます）。
   公式のコンテナイメージでは `/opt/senca/lib`（利用者が足すライブラリの置き場所。[16. コンテナイメージ](16-containers.md)）を指定しています。
2. 見つからなければ、システム既定の dlopen の検索順（`LD_LIBRARY_PATH` を含む）。

読み込むのはプロバイダーごとに決まった soname だけで、API からライブラリのパスを指定することはできません。
特定のプロバイダーを使わせないときは、ノードの環境変数 `SENCA_CODEC_DISABLE` に、`,` 区切りでプロバイダー名（`nvidia`・`intel-vpl`・`amd-amf`・`vaapi`・
`dav1d`・`libvpx`・`svt-av1`・`opus`）か、`hardware`・`software` を指定します。

インストールできたかどうかは、`GET /api/v1/capabilities` の `codecs.transcode` と `GET /api/v1/devices` で確認できます（§14.8）。

## 14.6 codec worker

- 実行ファイル `senca-codec-worker` を `senca-node` と同じディレクトリに置きます（別の場所に置く場合は `--codec-worker-bin FILE` で指定します）。
- 短いジョブ（静止画・サムネイル）用の worker の数は `--codec-workers N`（0 または省略で CPU のコア数）です。パイプラインは 1 つにつき 1 プロセスを使います。
- 1 つのジョブのタイムアウトは 30 秒です。worker が異常終了したら起動し直します。
- worker が見つからない、または起動しない場合、ノードは `codec worker unavailable: ...` をログに出力して動作を続けます。このとき、静止画・サムネイル・
  パイプラインは使えず（503 になるか、作成されません）、`GET /capabilities` の `pipelines` は `false`、`codecs.transcode` は空になります。

### サンドボックス（Linux）

worker は、適用した制限を起動時のログに出力します（例: `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`）。

- 親プロセスが終了したら一緒に終了します。ptrace されないようにし（`PR_SET_DUMPABLE=0`）、core ファイルを作らず、`PR_SET_NO_NEW_PRIVS` を設定します。
- Landlock（対応するカーネルの場合）: ファイルの作成・書き込み・削除を、`/dev` の下（GPU のデバイスファイル）以外では禁止します。読み取りは許可します（ライブラリを dlopen するため）。
- seccomp の拒否リスト: ネットワーク（socket・connect・bind・listen・accept）、プログラムの実行、ptrace、mount、カーネルモジュール、bpf、名前空間など。
- ジョブごとの CPU 時間の上限（超えると worker は終了し、起動し直されます）。
- ネットワークを禁止しているため、NVIDIA の persistenced への接続など、ドライバーの一部の補助機能は使えません。

詳しくは [12. セキュリティ](12-security.md) を見てください。

## 14.7 `codec_profile: royalty_free`

ストリームの `codec_profile` を `royalty_free` にすると、そのストリームでは特許プールのあるコーデック（H.264・H.265・AAC）を扱いません。

- 取り込みで H.264・H.265・AAC のトラックが届くと、**そのトラックを破棄します**（ログに `rejected by royalty_free profile; dropping track`）。
  配信者の接続は切りません。映像が H.264 の場合は、映像の無い配信になります。
- `hls.container` は `fmp4` にする必要があります（`ts` だと作成・変更が 400 になります。AV1・VP9 は TS に格納できないためです）。
- パイプラインでは、`source`・`sink` のストリームのどれかが `royalty_free` なら、H.264・H.265・AAC の使用（パススルーを含む）は `POLICY_REJECTED` になります。
- 静止画・サムネイル・キャプチャのエンコードも、この方針に従ってプロバイダーを選びます。
- 配信者には、AV1・VP9・VP8 と Opus で送信してもらってください（WHIP・Enhanced RTMP。[4. 取り込みと再生](04-ingest-playback.md)）。
- ストリームの処理では再エンコードしないため、配信計画（`POST /streams/{id}/plan`）に変換は出ません。`royalty_free` のストリームに届いた
  H.264・H.265・AAC のトラックは、計画ではすべての出力で `unsupported`（理由付き）になります。AV1 などへ変換したい場合は、
  パイプライン（[7. パイプライン](07-pipelines.md)）の `decode`・`encode` で別のストリームに出力してください（H.264・H.265 のデコードには GPU が必要です。§14.3）。

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"rf1","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
```

## 14.8 `GET /capabilities` と `GET /devices`

`GET /api/v1/capabilities`（認証されていれば権限は問いません）は、このノードでできることを返します。

```json
{
  "api_version": "v1",
  "codecs": {
    "passthrough": ["h264","h265","av1","vp8","vp9","aac","opus","pcma","pcmu"],
    "transcode": [
      {"provider":"dav1d","codec":"av1","direction":"decode","hardware":false,"max_width":16384,"max_height":16384,"max_sessions":null,"device":"dav1d"},
      {"provider":"libvpx","codec":"vp8","direction":"encode","hardware":false,"max_width":16383,"max_height":16383,"max_sessions":null,"device":"libvpx"}
    ]
  },
  "codec_profiles": ["any","royalty_free"],
  "pipelines": true
}
```

（抜粋。GPU の無いマシンに、ロイヤリティフリーのライブラリをインストールした場合の例です）

- `codecs.transcode` は、codec worker が実際に使えるプロバイダーの一覧です（プロバイダー・コーデック・方向・ハードウェアかどうか・最大解像度・同時セッション数・デバイス名）。
- ほかに `ingest`・`output`・`recordings`・`schedules`・`inputs`・`outputs`・`paas_providers`・`captions`・`rtsp_play`・`cluster` があります（[15. 管理 API と SDK](15-api-and-sdks.md)）。

`GET /api/v1/devices`（`node:admin`）は、プロバイダーごとの状態と理由、サンドボックス、キャプチャデバイスを返します。

```json
{"nodes": [{"node": "…",
  "devices": {"providers": [{"provider": "nvidia", "available": false, "detail": "…libcuda.so.1…"},
                            {"provider": "dav1d", "available": true, "detail": "…"}],
              "sandbox": "…"},
  "capture": {"available": false, "reason": "senca-capture-worker がありません"}}]}
```

`available: false` のときは、`detail` にライブラリやデバイスが見つからない理由が表示されます。

## 14.9 ライセンスについての注意

- ハードウェアのコーデックは、ベンダーがライセンス料を支払った製品の機能として提供されています。しかし、**配信サービスやコンテンツ配布に関わる特許料**
  （例: Via LA の AVC/HEVC、Access Advance）は、ハードウェアの購入だけでは済まない場合があります。
- AAC のパススルーの扱いや、AV1・VP8・VP9・Opus について第三者の特許プールが権利を主張している状況など、確認すべき点は
  [ライセンス・特許の台帳](../../licenses/README.md) にまとめています。SENCA は AV1 などを「特許がない」とは表示しません。
- ご自身のサービス・地域・コンテンツについては、利用者ご自身で確認してください。
