# 4. 取り込みと再生

[English](../en/04-ingest-playback.md)

送り手（エンコーダー・カメラ・ブラウザー）から stream に映像・音声を入れる方法と、視聴者に配る方法をまとめる。

- 設計の理由: [ADR-0006 stream タスク](../../dev/adr/0006-stream-hub.md)、[ADR-0012 WebRTC](../../dev/adr/0012-webrtc.md)、[ADR-0014 中継](../../dev/adr/0014-relay.md)、[ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)
- 関係するページ: [3. 設定](03-configuration.md)、[5. 録画](05-recording.md)、[6. 入力と出力](06-inputs-outputs.md)、[8. 字幕](08-captions.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)
- 実装: `crates/senca-node/src/rtmp.rs`・`srt.rs`・`webrtc.rs`・`capture.rs`・`http.rs`・`live.rs`・`packager.rs`・`rtsp_serve.rs`・`api/streams.rs`

## 4.1 全体の形

stream は 1 つの配信の単位で、**送り手は同時に 1 つだけ**。送り手から来た AU（符号化済みのフレーム）は stream タスク（hub）で 1 回だけ
HLS のセグメントにされ、同じものがライブ HLS・録画・RTMP/SRT/RTSP/WHEP の視聴者・出力・字幕に配られる。

| 入れ方 | プロトコル | 認証 |
|---|---|---|
| push（送り手がつなぐ） | RTMP・RTMPS・SRT（listener）・WHIP | stream の publish 鍵（`sk_...`） |
| pull（SENCA が取りに行く） | RTMP・RTSP・SRT（caller）・HLS | 入力の定義（[6. 入力と出力](06-inputs-outputs.md)） |
| キャプチャ機器 | 入力の `protocol: "device"` | 入力の定義（§4.7） |

| 配り方 | URL の元 | 必要な設定 |
|---|---|---|
| HLS（TS / fMP4、LL-HLS） | 署名付き URL の `hls` | `--http`（`--public-http-url`） |
| RTMP play | 署名付き URL の `rtmp` | `--rtmp`（`--public-rtmp-url`） |
| SRT play | `play:{stream_id}?exp=..&sig=..`（§4.12） | `--srt` |
| RTSP play | 署名付き URL の `rtsp` | `--rtsp` と `--public-rtsp-url` |
| WHEP | 署名付き URL の `whep` | `--webrtc-udp` |

以下の例は次の変数を使う。`TOKEN` は管理 API のトークン（単一ノードの初回起動では `{data-dir}/control/bootstrap-admin-token`）。

```sh
API=http://127.0.0.1:8080/api/v1
TOKEN=$(cat /var/lib/senca/control/bootstrap-admin-token)
H=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
```

**公開 URL を必ず設定する。** API の応答に出る URL（`ingest.*`、再生 URL）は待ち受けのアドレスではなく `--public-rtmp-url`・
`--public-http-url`・`--public-srt-url`・`--public-rtsp-url` から作る。`--public-http-url` の既定は `http://127.0.0.1:8080`、
`--public-rtmp-url` の既定は `rtmp://127.0.0.1:1935` で、待ち受けを変えても追従しない。`--public-srt-url` と `--public-rtsp-url` は
既定が無く、無ければ `ingest.srt_url`・再生 URL の `rtsp` は出ない（[3. 設定](03-configuration.md)）。

## 4.2 stream を作る

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"cam1","recording":{"enabled":true}}'
```

応答（抜粋）:

```json
{
  "id": "189f0fe84d8fc18358226d7380800f32",
  "name": "cam1",
  "observed_state": "waiting_input",
  "ingest": {
    "rtmp_url": "rtmp://127.0.0.1:24935/live",
    "stream_key": "sk_QE7_qJh12KNHNNfYUI19WlUT4VqV02XOwYDUcib3qL8",
    "srt_url": "srt://127.0.0.1:24900",
    "srt_streamid_format": "publish:{stream_key}",
    "whip_url": "http://127.0.0.1:24080/whip/189f0fe84d8fc18358226d7380800f32"
  },
  "live": null
}
```

- `stream_key` は**作成と鍵の作り直し（`POST /streams/{id}/keys`）の応答でだけ**値が入る。それ以外の取得では `null`。
  SENCA は鍵の SHA-256 だけを持つので、なくしたら作り直す（古い鍵はその時点で使えなくなる）。
- `Idempotency-Key` 付きの POST を再送したときの応答では、鍵は `null` で `stream_key_redacted: true` になる。
- `srt_url`・`srt_streamid_format` は `--public-srt-url` があるときだけ、`whip_url` は公開 HTTP URL があるときに出る。
- `observed_state` は `waiting_input`（送り手なし）・`live`・`stopped`（`desired_state: stopped`）。`live` には送り手の情報が入る:

```json
"live": {"transport": "rtmp", "epoch": 0, "started_at": "2026-11-03T01:15:53.255Z", "bitrate_bps": 956271, "viewers_hint": 0,
         "tracks": [{"track": 0, "codec": "h264", "width": 640, "height": 360},
                    {"track": 1, "codec": "aac", "sample_rate": 48000, "channels": 1}]}
```

`epoch` は同じ stream で送り手がつなぎ直すたびに増える（§4.16）。

## 4.3 stream のフィールド

作成（`POST /streams`）と変更（`PATCH /streams/{id}`、`If-Match` 必須）で使う。知らないフィールドは 400。

| フィールド | 既定 | 検査 | 意味 |
|---|---|---|---|
| `name` | （必須） | 1〜64 文字、英数字と `-` `_` `.`、`.` で始めない。テナント内で一意（重複は 409） | 表示名 |
| `latency_profile` | `standard` | `standard`・`low`・`interactive` | `low` で LL-HLS の part を出す（§4.9）。`interactive` は今は `standard` と同じ扱い |
| `codec_profile` | `any` | `any`・`royalty_free` | `royalty_free` は特許プールのあるコーデック（H.264・H.265・AAC）のトラックを捨てる。`hls.container` は `fmp4` が必須（[14. コーデックと特許](14-codecs-and-patents.md)） |
| `hls.segment_ms` | `2000` | 500〜10000 | セグメントの目標長（ミリ秒）。実際の区切りはキーフレーム |
| `hls.window` | `6` | 3〜60 | ライブの再生リストに載せるセグメント数 |
| `hls.container` | `ts` | `ts`・`fmp4` | セグメントの入れ物。録画も同じ入れ物になる |
| `recording.enabled` | `false` | | publish が始まったら自動で録画する（[5. 録画](05-recording.md)） |
| `recording.retention_days` | `null` | 1〜36500 | 自動録画の保持日数（`null` は無期限） |
| `snapshots` | `null` | `interval_secs` 1〜86400、`keep` 1〜10000 | 一定間隔で静止画を保存する（§4.15） |
| `limits.max_viewers` | `null` | | 保存されるが、今のノードは使わない（制限しない） |
| `limits.max_ingress_bps` | `null` | | 同上 |
| `metadata` | `{}` | 32 個まで、キー 1〜64 文字、値 1024 文字まで | 利用者の付箋（文字列だけ） |
| `desired_state` | `running` | `running`・`stopped` | `stopped` の間は publish を拒否する。配信中に `stopped` にすると送り手の受け付けを止め、その場で配信を終える |
| `captions` | `null` | [8. 字幕](08-captions.md) | 音声認識・翻訳の設定 |

PATCH は書いたフィールドだけを置き換える。`hls`・`recording`・`limits`・`metadata` は**オブジェクトごと置き換える**
（`{"hls":{"segment_ms":1000}}` は `window`・`container` を既定に戻す）。`snapshots: null`・`captions: null` で消せる。

```sh
ETAG=$(curl -s -D - -o /dev/null "$API/streams/$SID" -H "Authorization: Bearer $TOKEN" | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" \
  -d '{"latency_profile":"low","hls":{"segment_ms":2000,"window":6,"container":"fmp4"}}'
```

`hls`・`latency_profile` の変更は、次の publish から効く（今のパッケージャーは作り直さない）。

## 4.4 RTMP / RTMPS で送る

- URL: `ingest.rtmp_url`（`{public-rtmp-url}/live`）。stream name（stream key）に `stream_key` を入れる。
- application 名は `live`、`live` で始まる名前、`play` を受け付ける。それ以外は connect を拒否する。
- stream key の `?` 以降は無視する。
- 送り手が同時に 2 つになると、後から来た方を `NetStream.Publish.BadName`（already publishing）で拒否する。
- 鍵の誤り・`stopped` は `NetStream.Publish.BadName`、`live` 予約の窓の外（[5. 録画](05-recording.md)）は `NetStream.Publish.Rejected` で拒否する。
- 30 秒何も受けないと切る。

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f flv "rtmp://media.example.com:1935/live/$KEY"
```

OBS では「サーバー」に `rtmp://media.example.com:1935/live`、「ストリームキー」に `sk_...` を入れる。

**RTMPS**: `--rtmps ADDR` と `--tls-cert`・`--tls-key` で TLS の待ち受けを開く。URL は `rtmps://{host}:{port}/live`、鍵は同じ。
`ingest.rtmp_url` は RTMP の公開 URL から作るので、RTMPS の URL は送り手側で組み立てる。

**コーデック**（FLV・Enhanced RTMP v2）:

| 種類 | 従来の FLV | Enhanced RTMP（FourCC） |
|---|---|---|
| 映像 | H.264（CodecID 7）、H.265（非公式の CodecID 12） | `avc1`（H.264）・`hvc1`（H.265）・`av01`（AV1）・`vp09`（VP9）・`vp08`（VP8） |
| 音声 | AAC（10）、G.711 A-law（7）・μ-law（8） | `mp4a`（AAC）・`Opus` |

Enhanced RTMP の Multitrack はまだ扱わない（その接続はエラーで切れる）。それ以外の音声（MP3 など）も切れる。

## 4.5 SRT で送る（listener）

`--srt ADDR`（UDP）で SRT の listener を開く。SENCA は listener、送り手は caller。

- Stream ID: `publish:{stream_key}`、または SRT の Access Control 形式 `#!::r={stream_key},m=publish`。
- 中身は MPEG-TS。TS に入るコーデック（H.264・H.265・AAC・Opus）を送る。
- listener の受信遅延（latency）は 200 ms。
- 鍵の誤りは接続の段階で拒否する（`X_FORBIDDEN`）。

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY"
```

**暗号（パスフレーズ）**: テナントに `srt-passphrase` という名前の秘密値があると、そのテナントの stream への SRT 接続
（publish と play の両方）はそのパスフレーズの暗号を必須にする。パスフレーズの無い接続・違う接続は拒否する（平文へ黙って下げない）。
秘密値が無いテナントでは暗号を使わない。パスフレーズは SRT の決まりで 10〜79 文字。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"srt-passphrase","value":"correct-horse-battery"}'
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY&passphrase=correct-horse-battery"
```

秘密値の作成には `tenant:admin` が要る。SENCA が外へつなぐ SRT（`srt_pull` 入力・`srt_push` 出力）のパスフレーズはこれとは別で、
入力・出力の `credential_ref` で渡す（[6. 入力と出力](06-inputs-outputs.md)）。

## 4.6 WHIP で送る（WebRTC）

`--webrtc-udp ADDR` で WebRTC を有効にする（ICE-lite、1 つの UDP ソケットを全セッションで共有）。ICE の候補には待ち受けの
アドレスがそのまま載るので、`0.0.0.0` ではなく送り手から届く IP を指定する。

| 操作 | 要求 | 応答 |
|---|---|---|
| 開始 | `POST /whip/{stream_id}`、`Authorization: Bearer {stream_key}`、`Content-Type: application/sdp`、本文は offer | 201、本文は answer（`application/sdp`）、`Location: /whip/{stream_id}/{session}` |
| trickle ICE・ICE restart | `PATCH {Location}`、`Content-Type: application/trickle-ice-sdpfrag` | 204、または 200 と sdpfrag（ICE restart） |
| 終了 | `DELETE {Location}` | 200 |

- パスは stream の **ID**（名前ではない）。`ingest.whip_url` がそのまま使える。
- 鍵が違えば 401、Content-Type が違えば 415、offer が受けられなければ 400、`--webrtc-udp` の無いノードでは 404。
- `PATCH`・`DELETE` は推測できない 64 bit のセッション ID だけで受け付ける（`Location` を漏らさない）。
- CORS: `OPTIONS` に答え、`Location`・`ETag` を公開する（ブラウザーから送れる）。
- 受けるコーデック: H.264（Constrained Baseline・High、packetization-mode 1）・VP8・VP9 profile 0・AV1・Opus・PCMU・PCMA。
  サイマルキャストは最初の層だけを使う。
- HLS を区切るため、SENCA は 2 秒ごとに送り手にキーフレームを求める（PLI）。
- ブラウザーから送るページ（`apps/sender-web`）は `--sender-dir` を設定すると `/sender/` で配られる。

```sh
curl -s -X POST "http://media.example.com:8080/whip/$SID" -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/sdp" --data-binary @offer.sdp -D headers.txt -o answer.sdp
```

## 4.7 キャプチャ機器から入れる

ノードにつないだカメラ・キャプチャカードは、入力（`protocol: "device"`）として stream に入れる。機器の SDK はノードに読み込まず、
別プロセスの `senca-capture-worker`（`senca-node` と同じディレクトリに置く）が取り込み・符号化し、符号化済みの AU だけをノードに渡す。

使える機器は `GET /api/v1/devices` の `nodes[].capture` に出る（`node:admin` が要る）。

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{
  "protocol": "device",
  "url": "device:v4l2:/dev/video0",
  "options": {"codec": "vp9", "width": "1280", "height": "720", "fps": "30", "bitrate_bps": "2500000",
              "audio_device": "alsa:hw:1,0", "audio_codec": "opus"}
}'
```

| 項目 | 既定 | 意味 |
|---|---|---|
| `url` | （必須） | `device:{機器 ID}`（例: `device:v4l2:/dev/video0`・`device:decklink:0`）。300 文字まで |
| `options.codec` | `vp9` | 映像の符号化。H.264/H.265 を選ぶと GPU の符号化が要る（グリッドが GPU のセッションを数える） |
| `options.width`・`height`・`fps` | 1280・720・30 | |
| `options.bitrate_bps` | 2500000 | |
| `options.audio_device` | なし | 書いたときだけ音声も取る |
| `options.audio_codec` | `opus` | |

options の値はすべて文字列。stream が `royalty_free` なら、取り込みの worker もその方針で符号化を選ぶ。キャプチャの入力は
機器を持つノードに置かれる（グリッドの仕事。[9. クラスタとグリッド](09-cluster-grid.md)）。入力の状態とつなぎ直しは
[6. 入力と出力](06-inputs-outputs.md) と同じ。`--capture-synthetic` は試験用の合成の機器（`device:synthetic:0`）を使う。

## 4.8 再生 URL（署名付き）

再生は `/api/v1` の外の URL で、Authorization ヘッダーを使わない。URL に期限と HMAC 署名が入る。

```sh
curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" -d '{"ttl_secs":600}'
```

```json
{
  "expires_at": "2026-11-03T01:26:00Z",
  "playback": {
    "hls":  "http://127.0.0.1:24080/s/1793669160/MGO1aub_Vkp8D2OwIHZjGw/live/189f0fe8.../index.m3u8",
    "rtmp": "rtmp://127.0.0.1:24935/play/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw",
    "whep": "http://127.0.0.1:24080/whep/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw",
    "rtsp": "rtsp://127.0.0.1:24554/live/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw"
  }
}
```

- 本文は省略できる。`ttl_secs` の既定は 300、範囲は 1〜`--max-playback-ttl`（既定 86400）。権限は `subscribe`。
- `rtsp` は `--public-rtsp-url` があるときだけ。`whep` は `--webrtc-udp` の無いノードでは `null`。
  `moq`（MoQ の WebTransport の URL。[4.15](#415-moq-と-http3) を参照）は、`--quic` を指定したノードだけに入ります。
- 署名は `live/{stream_id}/{exp}` の HMAC-SHA256（先頭 16 バイト、base64url）。ファイル名は署名に含まれないので、
  同じ `/s/{exp}/{sig}/live/{id}/` の下の任意のファイル（セグメント・`master.m3u8`・字幕）に使える。
- 期限切れ・署名の誤りは 403。鍵は制御状態にあり、クラスタの全ノードで同じ（どのノードでも検証できる）。
- 発行済みの URL は期限まで取り消せない。短い `ttl_secs` を使う（[12. セキュリティ](12-security.md)）。

## 4.9 HLS

| パス | 中身 |
|---|---|
| `/s/{exp}/{sig}/live/{id}/index.m3u8` | ライブの再生リスト |
| `…/{seq}.ts`・`…/{seq}.m4s` | セグメント（`hls.container` による） |
| `…/{seq}.{part}.m4s`（`.ts`） | LL-HLS の part |
| `…/init{n}.mp4` | fMP4 の初期化セグメント（設定が変わるたびに番号が進む） |
| `…/master.m3u8`・`sub_{lang}.m3u8`・`sub_{lang}_{n}.vtt` | 字幕があるとき（[8. 字幕](08-captions.md)） |

- セグメントはメモリから返し、ディスクを通らない。窓から外れたセグメントもしばらく（4 本分）取れる。
- 再生リストは `Cache-Control: no-cache`、セグメントは `max-age=60`。応答に `Access-Control-Allow-Origin: *` が付く。
- セグメントの番号は送り手がつなぎ直しても続きから進む（再生リストの `EXT-X-MEDIA-SEQUENCE` が戻らない）。
- 配信が終わる（再接続の猶予が切れる）と、再生リストに `#EXT-X-ENDLIST` が付く。
- 入れ物に入れられないコーデックは HLS に出ない。TS: H.264・H.265・AAC・Opus。fMP4: H.264・H.265・AV1・VP9・AAC・Opus。
  `POST /streams/{id}/plan` で、今の入力が各出力でどう扱われるか（remux・unsupported と理由）を確かめられる。

**LL-HLS**（`latency_profile: "low"`）: part の目標長は `min(500 ms, segment_ms / 4)`（200 ms 以上）。再生リストには
`EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=…,PART-HOLD-BACK=…`・`EXT-X-PART-INF`・`EXT-X-PART`・
`EXT-X-PRELOAD-HINT` が出る。

| クエリ | 動き |
|---|---|
| `_HLS_msn=N` | セグメント N が完成するまで待ってから返す（ブロッキング・リロード） |
| `_HLS_msn=N&_HLS_part=P` | セグメント N の part P ができるまで待つ |
| `_HLS_skip=YES`（または `v2`） | delta の再生リスト（古いセグメントを `EXT-X-SKIP` で省く） |

待つのは最長 6 秒で、間に合わなければ 503（`Retry-After: 1`）。作っている途中のセグメントより 2 つ以上先の番号、`_HLS_part` が 100 以上、数でない値は 400。
配信が終わっていればすぐ返す。

実行例（fMP4・LL-HLS、`segment_ms` 2000）:

```
#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=12.000,PART-HOLD-BACK=1.500
#EXT-X-PART-INF:PART-TARGET=0.500
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MAP:URI="init0.mp4"
…
#EXT-X-PART:DURATION=0.467,URI="6.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI="6.1.m4s"
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="6.2.m4s"
```

## 4.10 試す: RTMP で送って HLS で見る

次の手順はこの文書の作成時に、ローカルのノードで実行して確かめた。

```sh
# 1. stream を作る
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"cam1"}' > s.json
SID=$(python3 -c "import json;print(json.load(open('s.json'))['id'])")
KEY=$(python3 -c "import json;print(json.load(open('s.json'))['ingest']['stream_key'])")

# 2. テスト信号を RTMP で送る（別の端末で）
ffmpeg -re -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -g 60 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "rtmp://127.0.0.1:1935/live/$KEY"

# 3. 再生 URL を取って見る
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
curl -s "$HLS"            # #EXTM3U … 0.ts 1.ts …
ffplay "$HLS"             # または ffprobe / ブラウザーの hls.js
```

`-g 60`（30 fps で 2 秒）のようにキーフレームの間隔をセグメントの長さに合わせる。セグメントはキーフレームでしか区切れない。

## 4.11 RTMP play

署名付き URL の `rtmp`（`rtmp://{host}/play/{stream_id}?exp=..&sig=..`）をそのまま使う。application は `play`、
stream name は `{stream_id}?exp=..&sig=..`。署名の誤り・期限切れは `NetStream.Play.StreamNotFound`。

```sh
ffplay "rtmp://127.0.0.1:1935/play/$SID?exp=$EXP&sig=$SIG"
```

送るのは FLV（Enhanced RTMP を含む）。遅い視聴者は送信待ちが 8 MiB を超えると次のキーフレームまで落とす。

## 4.12 SRT play

Stream ID は `play:{stream_id}?exp={exp}&sig={sig}`（署名は再生 URL と同じ）、または `#!::r={stream_id}?exp=..&sig=..,m=request`。
中身は MPEG-TS（1316 バイトずつ）。テナントに `srt-passphrase` があればパスフレーズが要る。

URL の `streamid=` に入れるときは、値の中の `?`・`&`・`=` をパーセント符号化する（しないと `&` で切れて署名が届かず拒否される）。

```sh
ffplay "srt://127.0.0.1:9000?streamid=play:$SID%3Fexp%3D$EXP%26sig%3D$SIG"
```

`EXP`・`SIG` は再生 URL の `rtmp` などのクエリから取る。TS に入らないコーデック（AV1・VP8・VP9）はこの経路では出ない。

## 4.13 RTSP play

`--rtsp ADDR` で RTSP の待ち受けを開き、`--public-rtsp-url` を設定すると再生 URL に `rtsp` が出る。

- URL: `rtsp://{host}:{port}/live/{stream_id}?exp={exp}&sig={sig}`
- 転送は **TCP interleaved だけ**。UDP の SETUP は 461 で断る。プレイヤーに TCP を指定する（ffmpeg は `-rtsp_transport tcp`）。
- 署名の誤りは DESCRIBE で 404、送り手がいないか RTP にできるトラックが無ければ 415。

```sh
ffplay -rtsp_transport tcp "rtsp://127.0.0.1:8554/live/$SID?exp=$EXP&sig=$SIG"
```

## 4.14 WHEP

`POST {whep の URL}`（`Content-Type: application/sdp`、本文は offer）→ 201 と answer、`Location: /whep/{stream_id}/{session}`。
`PATCH`・`DELETE` は WHIP と同じ。

- 申し出るのは stream が今持っているコーデックだけ: H.264・VP8・VP9・AV1・Opus・PCMA・PCMU。
- **AAC の音声は WebRTC で運べないので出さない**（AAC の復号は特許の方針で行わない。WHEP で音声も要るなら送り手で Opus にする）。
  運べるトラックが 1 つも無ければ 422。
- 署名の誤り・期限切れは 403。
- つながるまでの間は直近の GOP をためておき、つながったら送る（キーフレームから始まる）。

## 4.15 MoQ と HTTP/3

`--quic ADDR`（と `--tls-cert`・`--tls-key`）を指定すると、一つの UDP ポートで QUIC v1 を受け付けます。既定では無効です。
接続ごとに ALPN で中身が決まります。

| ALPN | 中身 |
|---|---|
| `h3` | HTTP/3。TCP の HTTP と同じ経路を提供します（`/api/v1`、LL-HLS のブロッキング・リロードを含む署名付き HLS、WHIP/WHEP、`/healthz` など）。SSE（`/api/v1/events`）と `/signaling`（WebSocket）は 501 を返すので、HTTP/1.1 を使ってください。同じ接続で `/moq` への拡張 CONNECT を送ると、WebTransport の MoQ になります |
| `moqt-22` | native QUIC の MoQ（draft-ietf-moq-transport-22） |

- `--quic` を指定すると、HTTP・HTTPS の応答に `Alt-Svc: h3=":ポート"; ma=86400` が付き、ブラウザーは次の要求から HTTP/3 を使えます。
- 証明書はノードの `--tls-cert`・`--tls-key`（TCP の HTTPS と同じもの）を使います。
- MoQ は Internet-Draft のため、版は draft-22 に固定しています（`crates/senca-moq/DRAFT.md`）。他の版の client は、
  ALPN または `wt-available-protocols` の交渉の段階で拒否します。

### 購読（MoQ で視聴する）

ライブ中の stream は、すべて namespace `senca/live/{stream_id}` として購読できます（別のノードが持つ stream は、
中継のミラーから配信します）。

| track | 中身 |
|---|---|
| `catalog.json` | hang 形式の catalog（moq-dev/moq と同じ形式）。更新ごとに group 一つ・object 一つです。`video`・`audio` の `renditions` に `codec`（RFC 6381）、`description`（avcC などの 16 進）、`container: {"kind":"cmaf","init": base64(ftyp+moov)}` が入ります |
| `video`・`audio`（2 本目以降は `video2` など） | CMAF。object 一つが fragment 一つ（moof+mdat、サンプル一つ）です。映像は GOP ごと、音声はおよそ 1 秒ごとに新しい group になります |

- 認可には再生 URL と同じ署名（`exp`・`sig`）を使います。次のいずれかで渡してください。
  - MoQ の AUTHORIZATION TOKEN（SETUP または SUBSCRIBE）の値として `exp=..&sig=..`
  - native QUIC の PATH（`moqt://host:port/?exp=..&sig=..`）
  - WebTransport の URL のクエリー
- 署名が無い・誤っている・期限切れの SUBSCRIBE・FETCH・TRACK_STATUS は `UNAUTHORIZED` で拒否します。
  SUBSCRIBE_NAMESPACE には、署名が有効な stream だけを通知します。
- `--quic` を指定したノードでは、再生 URL（4.8）の `moq` に WebTransport の URL（`https://{host}:{port}/moq?exp=..&sig=..`）が
  入ります。host は `--public-http-url` の host です。
- 途中から購読した場合は、現在の group の先頭（キーフレーム）から配信します。draft-22 の FILL_PARAMETERS
  （current-group join）が指定されていれば、group の先頭部分を fill の stream で、続きを購読で配信します。
  直近の 2 group は FETCH でも取得できます。

moq-dev/moq の CLI で fMP4 に書き出す例:

```sh
Q="exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw"   # 再生 URL の moq のクエリー
moq --connect "moqt://media.example.com:4433/?$Q" --connect-version moq-transport-22 \
  --broadcast "senca/live/$SID" export fmp4 > live.mp4
```

### 送出（MoQ で送る）

stream の publish 鍵（`sk_...`）を AUTHORIZATION TOKEN（SETUP または PUBLISH_NAMESPACE）に入れて
`PUBLISH_NAMESPACE senca/live/{stream_id}` を送ると、ノードが `catalog.json` と catalog に載っている track を購読し、
その stream の送り手になります（`live.transport` は `moq` です）。

- 受け付ける容器は `cmaf`（init と fragment）だけです。hang の `legacy`・`loc` の track があると、拒否してセッションを閉じます
  （moq-dev の `moq import` は `legacy` を出力するため、そのままでは送れません）。
- 出版者起点の PUBLISH には対応していません（`NOT_SUPPORTED` を返します）。一つのセッションで送れる stream は一つです。
- 予約の「live」の時間帯の外では、他の送り方と同じく拒否します。

## 4.16 静止画

| 操作 | 要求 | 権限 | 結果 |
|---|---|---|---|
| 今の画 | `GET /streams/{id}/snapshot[?format=jpeg\|png]` | `stream:read` | 200 `image/jpeg`（既定）か `image/png` |
| 保存した一覧 | `GET /streams/{id}/snapshots` | `stream:read` | `{"items":[{id, taken_at, reason, width, height, bytes}]}`（古い順） |
| 1 枚 | `GET /streams/{id}/snapshots/{snapshot_id}` | `stream:read` | JPEG |
| 消す | `DELETE /streams/{id}/snapshots/{snapshot_id}` | `stream:write` | 204 |

- 静止画は直近の GOP を**復号**して作る。復号は codec worker で行い、H.264・H.265 は GPU のデコーダーが無いと 503
  `CAPACITY_UNAVAILABLE`（`retryable: false`）になる。AV1（dav1d）・VP8/VP9（libvpx）はソフトウェアで復号できる
  （[14. コーデックと特許](14-codecs-and-patents.md)）。配信中でなければ 409、キーフレーム待ちも 409。
- 保存は 3 つの理由（`reason`）で行う: `interval`（stream の `snapshots` 方針。`keep` 枚を超えたら古いものから消す）、
  `schedule`（`snapshot` 予約。[5. 録画](05-recording.md)）、`board_change`（パイプラインの `snapshot_on_change`。[7. パイプライン](07-pipelines.md)）。
- 保存先は `{data-dir}/snapshots/{stream_id}/`。stream を消すと一緒に消える。
- 録画のサムネイルは [5. 録画](05-recording.md)。

```sh
curl -s -o now.jpg "$API/streams/$SID/snapshot" -H "Authorization: Bearer $TOKEN"
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" -d '{"snapshots":{"interval_secs":60,"keep":1440}}'
```

## 4.17 送り手が切れたとき・入れ替わったとき

- 送り手が切れても、`--reconnect-grace`（既定 30000 ms）の間は配信を終えない。その間に同じ鍵でつなぎ直すと、
  `epoch` を 1 増やして同じ配信として続ける。HLS は `EXT-X-DISCONTINUITY` で区切り、セグメントの番号は続きから。
  録画は 1 本のまま（[5. 録画](05-recording.md)）。プロトコルが変わってもよい（RTMP で切れて SRT でつなぐなど）。
- 猶予が切れると配信の終わり: HLS に `ENDLIST`、RTMP/SRT/RTSP/WHEP の視聴者は閉じられ、録画は確定する。
- `desired_state: stopped` にすると送り手の受け付けをすぐ止め、猶予を待たずに配信を終える。

## 4.18 どのノードでも再生できる（クラスタ）

クラスタでは、送り手を受けたノードが stream の所有者になる。所有者でないノードに再生（HLS・RTMP/SRT/RTSP play・WHEP）が来ると、
そのノードは所有者から中継で AU を受けて手元に「写し」を作り、そこから配る。同じ stream の視聴者が何人いても上流は 1 本。
最初の HLS の要求では、写しの最初の再生リストができるまで最長 6 秒待つ。使われない写しは 30 秒で片付く。
写しは録画・字幕をしない（所有者のノードの仕事）。中継が使えないときは、所有者のノード（`media_addr`）へ 307 で案内する。
詳しくは [9. クラスタとグリッド](09-cluster-grid.md)。

## 4.19 よくある誤り

| 症状 | 原因 |
|---|---|
| RTMP の publish が拒否される | `BadName`: 鍵の誤り、すでに別の送り手がいる、`stopped`。`Rejected`: `live` 予約の窓の外 |
| SRT の接続がすぐ拒否される | Stream ID の形、鍵の誤り、テナントに `srt-passphrase` があるのにパスフレーズが無い（ログに `unsecure`） |
| SRT play が拒否される | `streamid` の値をパーセント符号化していない |
| HLS が 404 | まだ最初のセグメントができていない（キーフレームの間隔が長い）、URL のホストが違う（`--public-http-url`） |
| HLS に映像・音声が出ない | コーデックが入れ物に入らない（`POST /streams/{id}/plan`）、`royalty_free` で H.264/AAC を捨てた |
| 静止画が 503 | H.264/H.265 で GPU が無い、codec worker が起動していない |
| WHEP に音声が無い | 音声が AAC |
