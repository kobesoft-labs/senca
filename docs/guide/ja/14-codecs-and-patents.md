# 14. コーデックと特許

[English](../en/14-codecs-and-patents.md)

SENCA がどのコーデックをどう扱うか、何を製品に入れず、何を実行時に読み込むか。運用者が用意するもの（GPU のドライバー、
ロイヤリティフリーのコーデックのライブラリ）と、それが無いときの動きをまとめる。

- 設計の理由: [ADR-0004 コーデックの選定と特許の方針](../../dev/adr/0004-codecs-patents.md)、[ADR-0003 内部のメディア形式](../../dev/adr/0003-internal-media-format.md)
- 台帳: [ライセンス・特許の台帳](../../licenses/README.md)
- 関係するページ: [4. 取り込みと再生](04-ingest-playback.md)、[5. 録画](05-recording.md)、[7. パイプライン](07-pipelines.md)、[11. メディアパイプライン](11-media-pipelines.md)、[13. 運用](13-operations.md)
- 実装: `crates/senca-codec`（提供者・dlopen）、`crates/senca-codec-worker`（worker・サンドボックス）、`crates/senca-node/src/codec.rs`

> **法的な助言ではありません。** この章と台帳は SENCA の実装の方針を説明するものです。配信するコンテンツ・サービス・地域に
> どのライセンス（特許プールの料金を含む）が要るかは、利用者が自分で確かめてください。

## 14.1 方針

1. **H.264・H.265 の符号化・復号は、GPU ベンダーのハードウェアコーデックだけで行う。** ソフトウェアの H.264/H.265 コーデック
   （libavcodec、OpenH264、x264、x265 など）は製品に入れず、読み込まない。
2. **AAC はそのまま通すだけ。** AAC の符号化・復号（fdk-aac など）は入れない。音声を変換するときは Opus にする。
3. **ロイヤリティフリーとして提供されているコーデックはソフトウェアで使う**: dav1d（AV1 復号）、SVT-AV1（AV1 符号化）、
   libvpx（VP8/VP9）、libopus（Opus）。
4. 外部のコーデックのライブラリは製品に同梱せず、**別プロセスの `senca-codec-worker` が実行時に読み込む（dlopen）**。
   ノード本体はコーデックのライブラリを読み込まない。
5. 必要な装置・ライブラリが無いときは、黙って劣化させず、理由付きで断る（503 `CAPACITY_UNAVAILABLE` や検証のエラー）。

H.264/H.265 の**ビットストリームの解析・形式変換**（Annex B ⇔ 長さ前置、SPS/PPS/VPS の解析、RTP のパケット化、TS・fMP4・FLV への格納）は
コーデック（符号化・復号）ではなく、SENCA が自前で行う。

## 14.2 素通し（passthrough）はいつでもできる

符号化・復号をしない経路は、GPU もコーデックのライブラリも要らない。

| 経路 | 素通しできるコーデック |
|---|---|
| 取り込み（RTMP・SRT・WHIP・pull） | H.264・H.265・AV1・VP8・VP9・AAC・Opus・G.711（経路ごとの対応は [4. 取り込みと再生](04-ingest-playback.md)） |
| HLS（TS） | H.264・H.265・AAC・Opus |
| HLS（fMP4） | H.264・H.265・AV1・VP9・AAC・Opus |
| 録画・VOD・MP4 の書き出し | HLS と同じ（同じセグメント） |
| RTMP play・`rtmp_push` | FLV・Enhanced RTMP で運べるもの |
| SRT play・`srt_push` | TS で運べるもの |
| RTSP play | RTP にできるもの |
| WHEP | H.264・VP8・VP9・AV1・Opus・PCMA・PCMU（**AAC は運べない**ので音声は出ない） |
| 字幕 | AAC・Opus の音声を詰め替えて送る（[8. 字幕](08-captions.md)） |

`POST /streams/{id}/plan` は、今の入力の各トラックが各出力でどう扱われるか（`remux`・`repacketize`・`passthrough`・`unsupported` と理由）を返す。
stream の処理は再エンコードしないので、計画に変換は出ない。`royalty_free` の stream に届いた H.264・H.265・AAC のトラックは破棄され、
計画ではすべての出力で `unsupported`（理由付き）になる。AV1 などへ変換したいときは、パイプライン（[7. パイプライン](07-pipelines.md)）の
`decode`・`encode` で別の stream に出す（H.264・H.265 の復号には GPU が要る。§14.3）。

## 14.3 符号化・復号が要る操作

| 操作 | 要るもの | 無いとき |
|---|---|---|
| 今の画 `GET /streams/{id}/snapshot` | 映像の復号 | 503 `CAPACITY_UNAVAILABLE`（`retryable: false`） |
| 静止画の保存（`snapshots` 方針・`snapshot` 予約） | 映像の復号 | 保存されない（ログに理由） |
| 録画のサムネイル | 映像の復号 | 作られない（`GET /recordings/{id}/thumbnail` は 404） |
| パイプライン（`decode`・`encode`） | 部品ごとの復号・符号化 | 作成・変更が 503 `CAPACITY_UNAVAILABLE` か 400 `POLICY_REJECTED`・`UNSUPPORTED_CODEC`（[7. パイプライン](07-pipelines.md)） |
| 部屋のメディアパイプラインの合成 | 参加者の映像の復号と合成の符号化（既定 AV1） | [11. メディアパイプライン](11-media-pipelines.md) |
| キャプチャの入力 | 取り込みの worker での符号化（既定 VP9） | 入力が `error`・`backoff` |

例: GPU の無いノードで H.264 の stream の今の画を求めたとき（実行して確かめた）:

```json
{"type":"urn:senca:error:capacity-unavailable","title":"h264 を復号できる装置がありません（H.264/H.265 はハードウェアのデコーダーだけを使います）",
 "status":503,"code":"CAPACITY_UNAVAILABLE","retryable":false}
```

VP9 の stream なら同じノードでも libvpx で復号でき、JPEG・PNG が返る。

## 14.4 ハードウェアの提供者

| 提供者（`provider`） | API | 読み込むライブラリ | 符号化 | 復号 |
|---|---|---|---|---|
| `nvidia` | NVENC・NVDEC（Video Codec SDK 13.1、Linux ドライバー 610 以上） | `libcuda.so.1`・`libnvidia-encode.so.1`・`libnvcuvid.so.1`（`libnvidia-ml.so.1` は版の表示だけ、無くてよい） | H.264・H.265・AV1 | H.264・H.265・AV1・VP9 |
| `intel-vpl` | oneVPL（Quick Sync） | `libvpl.so.2`（dispatcher がドライバーの実装を探す） | H.264・H.265・AV1 | H.264・H.265・AV1・VP9 |
| `amd-amf` | AMF（Linux は Vulkan） | `libamfrt64.so.1` | H.264・H.265・AV1 | H.264・H.265 |
| `vaapi` | VA-API | `libva.so.2`・`libva-drm.so.2`、`/dev/dri/renderD*` | なし | H.264 |

- どれもハードウェアの実装だけを選ぶ（oneVPL・AMF のソフトウェア実装は使わない）。
- ライブラリや装置が無いのは正常として扱い、その提供者を「使えない」とする（`GET /devices` に理由）。
- ハードウェアの経路は、開発機に GPU が無いため実機で未検証のものがある（各提供者のソースの「検証の状況」、REQ-004）。
  使う前に自分の GPU・ドライバーで確かめる。
- ADR-0004 の表にある Apple VideoToolbox の提供者は、今は実装されていない。

コンテナで GPU を使うときは、装置とドライバーのライブラリをコンテナに渡す（[13. 運用](13-operations.md)、`compose.yaml`）。

## 14.5 ロイヤリティフリーのソフトウェアコーデック

| 提供者 | ライブラリ（soname） | 用途 | ライセンス |
|---|---|---|---|
| `dav1d` | `libdav1d.so.7` | AV1 の復号 | BSD-2-Clause |
| `svt-av1` | `libSvtAv1Enc.so.2` | AV1 の符号化 | BSD-3-Clause＋AOM の特許許諾 |
| `libvpx` | `libvpx.so.12`（libvpx 1.16 の ABI） | VP8・VP9 の符号化・復号 | BSD-3-Clause＋WebM の特許許諾 |
| `opus` | `libopus.so.0` | Opus の符号化・復号 | BSD-3-Clause＋IETF の宣言の特許許諾 |

### 入れ方

ディストリビューションのパッケージを入れる。公式のコンテナ（`Dockerfile`）は Ubuntu 26.04 で次を入れている:

```sh
apt-get install -y --no-install-recommends libdav1d7 libvpx12 libopus0 libsvtav1enc2 libva2 libva-drm2 libvpl2
```

soname が合わない版（例: `libvpx.so.9` しか無い）は読み込まない。libvpx の提供者は libvpx 1.16（`libvpx.so.12`）の ABI に合わせてある。

探す場所:

1. `--codec-lib-dir DIR`（ノードが worker の環境変数 `SENCA_CODEC_LIB_DIR` に渡す。`:` 区切りで複数書ける）。
   公式のコンテナは `--codec-lib-dir /usr/lib/x86_64-linux-gnu` で起動する。
2. 見つからなければ、システムの既定の dlopen の検索順（`LD_LIBRARY_PATH` を含む）。

読み込むのは提供者ごとに決まった soname だけで、API からライブラリのパスを指定することはできない。
特定の提供者を使わせないときは、ノードの環境変数 `SENCA_CODEC_DISABLE` に `,` 区切りで提供者の名前（`nvidia`・`intel-vpl`・`amd-amf`・`vaapi`・
`dav1d`・`libvpx`・`svt-av1`・`opus`）か `hardware`・`software` を書く。

入ったかどうかは `GET /api/v1/capabilities` の `codecs.transcode` と `GET /api/v1/devices` で確かめる（§14.8）。

## 14.6 codec worker

- 実行ファイル `senca-codec-worker` を `senca-node` と同じディレクトリに置く（別の場所なら `--codec-worker-bin FILE`）。
- 短い仕事（静止画・サムネイル）用のプールの大きさは `--codec-workers N`（0 または省略で CPU のコア数）。パイプラインは 1 つにつき 1 プロセス。
- 仕事 1 つの時間切れは 30 秒。worker が落ちたら作り直す。
- worker が見つからない・起動しないと、ノードは `codec worker unavailable: ...` をログに出して動き続ける。静止画・サムネイル・
  パイプラインは使えず（503・作られない）、`GET /capabilities` の `pipelines` は `false`、`codecs.transcode` は空。

### サンドボックス（Linux）

worker は起動ログに適用した制限を書く（例: `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`）。

- 親が死んだら一緒に終わる、ptrace されない（`PR_SET_DUMPABLE=0`）、core を作らない、`PR_SET_NO_NEW_PRIVS`。
- Landlock（使えるカーネルで）: ファイルの作成・書き込み・削除を `/dev` の下（GPU の装置ファイル）以外で禁止。読み取りは許す（ライブラリの dlopen のため）。
- seccomp の拒否リスト: ネットワーク（socket・connect・bind・listen・accept）、プログラムの実行、ptrace、mount、カーネルモジュール、bpf、名前空間など。
- 仕事ごとの CPU 時間の上限（超えると worker は終わり、作り直される）。
- ネットワークを禁じるので、NVIDIA の persistenced への接続など、ドライバーの一部の補助機能は使えない。

## 14.7 `codec_profile: royalty_free`

stream の `codec_profile` を `royalty_free` にすると、その stream は特許プールのあるコーデック（H.264・H.265・AAC）を扱わない。

- 取り込みで H.264・H.265・AAC のトラックが来たら、**そのトラックを捨てる**（ログに `rejected by royalty_free profile; dropping track`）。
  送り手の接続は切らない。映像が H.264 なら映像の無い配信になる。
- `hls.container` は `fmp4` が必須（`ts` だと作成・変更が 400。AV1・VP9 は TS に入らないため）。
- パイプラインでは、`source`・`sink` の stream のどれかが `royalty_free` なら、H.264・H.265・AAC の使用（素通しも）が `POLICY_REJECTED`。
- 静止画・サムネイル・キャプチャの符号化も、この方針で提供者を選ぶ。
- 送り手には AV1・VP9・VP8 と Opus で送ってもらう（WHIP・Enhanced RTMP。[4. 取り込みと再生](04-ingest-playback.md)）。

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"rf1","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
```

## 14.8 `GET /capabilities` と `GET /devices`

`GET /api/v1/capabilities`（認証されていれば権限は問わない）は、このノードでできることを返す。

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

（抜粋。GPU の無い開発機で、ロイヤリティフリーのライブラリを入れて実行した結果）

- `codecs.transcode` は codec worker が実際に使える提供者の一覧（提供者・コーデック・向き・ハードウェアか・最大の解像度・同時セッション数・装置名）。
- 他に `ingest`・`output`・`recordings`・`schedules`・`inputs`・`outputs`・`paas_providers`・`captions`・`rtsp_play`・`cluster` がある（[15. API と SDK](15-api-and-sdks.md)）。

`GET /api/v1/devices`（`node:admin`）は、提供者ごとの状態と理由、サンドボックス、キャプチャ機器を返す。

```json
{"nodes": [{"node": "…",
  "devices": {"providers": [{"provider": "nvidia", "available": false, "detail": "…libcuda.so.1…"},
                            {"provider": "dav1d", "available": true, "detail": "…"}],
              "sandbox": "…"},
  "capture": {"available": false, "reason": "senca-capture-worker がありません"}}]}
```

`available: false` の `detail` に、ライブラリ・装置が見つからない理由が出る。

## 14.9 ライセンスについての注意

- ハードウェアのコーデックはベンダーが対価を払った製品の機能として提供されているが、**配信サービス・コンテンツ配布に関わる特許料**
  （例: Via LA の AVC/HEVC、Access Advance）は、ハードウェアの購入で済まない場合がある。
- AAC の素通しの扱い、AV1・VP8・VP9・Opus について第三者のプールが権利を主張している状況など、確認すべき点は
  [ライセンス・特許の台帳](../../licenses/README.md) にまとめてある。SENCA は AV1 などを「特許がない」とは表示しない。
- 自分のサービス・地域・コンテンツについては、利用者自身が確かめる。
