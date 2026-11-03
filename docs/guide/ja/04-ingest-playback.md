# 4. 取り込みと再生

[English](../en/04-ingest-playback.md)

このページでは、配信者（エンコーダー・カメラ・ブラウザー）からストリームに映像・音声を取り込む方法と、視聴者へ配信する方法を説明します。

- 設計の背景: [ADR-0006 stream タスク](../../dev/adr/0006-stream-hub.md)、[ADR-0012 WebRTC](../../dev/adr/0012-webrtc.md)、[ADR-0014 中継](../../dev/adr/0014-relay.md)、[ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)
- 関連するページ: [3. 設定](03-configuration.md)、[5. 録画](05-recording.md)、[6. 入力と出力](06-inputs-outputs.md)、[8. 字幕](08-captions.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)

## 4.1 全体の流れ

ストリームは 1 つの配信の単位で、**同時に接続できる配信者は 1 つだけ**です。配信者から届いた AU（エンコード済みのフレーム）は、
ストリームのタスク（hub）で 1 回だけ HLS のセグメントに変換されます。同じデータが、ライブの HLS・録画・RTMP/SRT/RTSP/WHEP の視聴者・
出力・字幕に配信されます。

取り込みの方法は次のとおりです。

| 取り込み方法 | プロトコル | 認証 |
|---|---|---|
| push（配信者が接続する） | RTMP・RTMPS・SRT（listener）・WHIP | ストリームキー（`sk_...`） |
| pull（SENCA が取得しに行く） | RTMP・RTSP・SRT（caller）・HLS | 入力の定義（[6. 入力と出力](06-inputs-outputs.md)） |
| キャプチャデバイス | 入力の `protocol: "device"` | 入力の定義（§4.7） |

配信の方法は次のとおりです。

| 配信方法 | URL の取得元 | 必要な設定 |
|---|---|---|
| HLS（TS / fMP4、LL-HLS） | 署名付き URL の `hls` | `--http`（`--public-http-url`） |
| RTMP play | 署名付き URL の `rtmp` | `--rtmp`（`--public-rtmp-url`） |
| SRT play | `play:{stream_id}?exp=..&sig=..`（§4.12） | `--srt` |
| RTSP play | 署名付き URL の `rtsp` | `--rtsp` と `--public-rtsp-url` |
| WHEP | 署名付き URL の `whep` | `--webrtc-udp` |

以下の例では、次の変数を使います。`TOKEN` は管理 API のトークンです（単一ノードの初回起動時は `{data-dir}/control/bootstrap-admin-token`）。

```sh
API=http://127.0.0.1:8080/api/v1
TOKEN=$(cat /var/lib/senca/control/bootstrap-admin-token)
H=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
```

**公開 URL は必ず設定してください。** API の応答に含まれる URL（`ingest.*`、再生 URL）は、待ち受けアドレスではなく、`--public-rtmp-url`・
`--public-http-url`・`--public-srt-url`・`--public-rtsp-url` から作られます。`--public-http-url` の既定は `http://127.0.0.1:8080`、
`--public-rtmp-url` の既定は `rtmp://127.0.0.1:1935` で、待ち受けアドレスを変えても自動では変わりません。`--public-srt-url` と
`--public-rtsp-url` には既定値がなく、指定しない場合は `ingest.srt_url` と再生 URL の `rtsp` が出力されません（[3. 設定](03-configuration.md)）。

## 4.2 ストリームを作成する

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

- `stream_key` に値が入るのは、**作成時とキーの再発行時（`POST /streams/{id}/keys`）の応答だけ**です。それ以外の取得では `null` になります。
  SENCA はキーの SHA-256 だけを保存しているため、キーをなくした場合は再発行してください（古いキーはその時点で使えなくなります）。
- `Idempotency-Key` 付きの POST を再送したときの応答では、キーは `null` になり、`stream_key_redacted: true` が付きます。
- `srt_url`・`srt_streamid_format` は `--public-srt-url` を指定したときだけ、`whip_url` は公開 HTTP URL があるときだけ出力されます。
- `observed_state` は `waiting_input`（配信者なし）・`live`・`stopped`（`desired_state: stopped`）のいずれかです。`live` には配信者の情報が入ります。

```json
"live": {"transport": "rtmp", "epoch": 0, "started_at": "2026-11-03T01:15:53.255Z", "bitrate_bps": 956271, "viewers_hint": 0,
         "tracks": [{"track": 0, "codec": "h264", "width": 640, "height": 360},
                    {"track": 1, "codec": "aac", "sample_rate": 48000, "channels": 1}]}
```

`epoch` は、同じストリームに配信者が再接続するたびに増えます（§4.17）。

## 4.3 ストリームのフィールド

作成（`POST /streams`）と変更（`PATCH /streams/{id}`、`If-Match` が必須）で使うフィールドです。未知のフィールドを指定すると 400 になります。

| フィールド | 既定 | 制約 | 説明 |
|---|---|---|---|
| `name` | （必須） | 1〜64 文字。英数字と `-` `_` `.` が使え、`.` で始めることはできません。テナント内で一意（重複は 409） | 表示名 |
| `latency_profile` | `standard` | `standard`・`low`・`interactive` | `low` にすると LL-HLS の part を出力します（§4.9）。`interactive` は現在 `standard` と同じ動作です |
| `codec_profile` | `any` | `any`・`royalty_free` | `royalty_free` にすると、特許プールのあるコーデック（H.264・H.265・AAC）のトラックを破棄します。`hls.container` は `fmp4` にする必要があります（[14. コーデックと特許](14-codecs-and-patents.md)） |
| `hls.segment_ms` | `2000` | 500〜10000 | セグメントの目標の長さ（ミリ秒）。実際にはキーフレームの位置で区切ります |
| `hls.window` | `6` | 3〜60 | ライブのプレイリストに含めるセグメントの数 |
| `hls.container` | `ts` | `ts`・`fmp4` | セグメントのコンテナ形式。録画も同じ形式になります |
| `recording.enabled` | `false` | | 配信が始まったら自動で録画します（[5. 録画](05-recording.md)） |
| `recording.retention_days` | `null` | 1〜36500 | 自動録画の保持日数（`null` は無期限） |
| `snapshots` | `null` | `interval_secs` 1〜86400、`keep` 1〜10000 | 一定の間隔で静止画を保存します（§4.16） |
| `limits.max_viewers` | `null` | | 値は保存されますが、現在は制限に使われません |
| `limits.max_ingress_bps` | `null` | | 同上 |
| `metadata` | `{}` | 32 個まで。キーは 1〜64 文字、値は 1024 文字まで | 利用者が自由に付けられるメタデータ（文字列のみ） |
| `desired_state` | `running` | `running`・`stopped` | `stopped` の間は配信を拒否します。配信中に `stopped` にすると、配信者からの受信を止め、その時点で配信を終了します |
| `captions` | `null` | [8. 字幕](08-captions.md) を参照 | 音声認識・翻訳の設定 |

PATCH では、指定したフィールドだけを置き換えます。`hls`・`recording`・`limits`・`metadata` は**オブジェクト全体が置き換わります**
（たとえば `{"hls":{"segment_ms":1000}}` を送ると、`window`・`container` は既定値に戻ります）。`snapshots: null`・`captions: null` で設定を削除できます。

```sh
ETAG=$(curl -s -D - -o /dev/null "$API/streams/$SID" -H "Authorization: Bearer $TOKEN" | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" \
  -d '{"latency_profile":"low","hls":{"segment_ms":2000,"window":6,"container":"fmp4"}}'
```

`hls`・`latency_profile` の変更は、次の配信開始から反映されます（配信中のパッケージャーは作り直しません）。

## 4.4 RTMP / RTMPS で配信する

- URL は `ingest.rtmp_url`（`{public-rtmp-url}/live`）です。RTMP の stream name（ストリームキー）に `stream_key` を指定します。
- application 名は `live`、`live` で始まる名前、`play` を受け付けます。それ以外の名前では connect を拒否します。
- ストリームキーの `?` 以降は無視します。
- 同じストリームに 2 つ目の配信者が接続した場合は、後から接続した方を `NetStream.Publish.BadName`（already publishing）で拒否します。
- キーの誤りと `stopped` の場合は `NetStream.Publish.BadName` で、`live` 予約の時間帯の外（[5. 録画](05-recording.md)）の場合は
  `NetStream.Publish.Rejected` で拒否します。
- 30 秒間何も受信しない場合は切断します。

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f flv "rtmp://media.example.com:1935/live/$KEY"
```

OBS では、「サーバー」に `rtmp://media.example.com:1935/live`、「ストリームキー」に `sk_...` を入力します。

**RTMPS**: `--rtmps ADDR` と `--tls-cert`・`--tls-key` を指定すると、TLS での待ち受けを開始します。URL は `rtmps://{host}:{port}/live`
で、キーは RTMP と同じです。`ingest.rtmp_url` は RTMP の公開 URL から作られるため、RTMPS の URL は配信者側で組み立ててください。

**対応コーデック**（FLV・Enhanced RTMP v2）:

| 種類 | 従来の FLV | Enhanced RTMP（FourCC） |
|---|---|---|
| 映像 | H.264（CodecID 7）、H.265（非公式の CodecID 12） | `avc1`（H.264）・`hvc1`（H.265）・`av01`（AV1）・`vp09`（VP9）・`vp08`（VP8） |
| 音声 | AAC（10）、G.711 A-law（7）・μ-law（8） | `mp4a`（AAC）・`Opus` |

Enhanced RTMP の Multitrack には現在対応していません（その接続はエラーで切断されます）。表にない音声コーデック（MP3 など）の場合も切断されます。

## 4.5 SRT で配信する（listener）

`--srt ADDR`（UDP）を指定すると、SRT の listener を開始します。SENCA が listener、配信者が caller になります。

- Stream ID は `publish:{stream_key}`、または SRT の Access Control 形式の `#!::r={stream_key},m=publish` です。
- 送信するデータは MPEG-TS です。TS に格納できるコーデック（H.264・H.265・AAC・Opus）を使ってください。
- listener の受信遅延（latency）は 200 ms です。
- キーが誤っている場合は、接続の段階で拒否します（`X_FORBIDDEN`）。

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY"
```

**暗号化（パスフレーズ）**: テナントに `srt-passphrase` という名前の秘密値があると、そのテナントのストリームへの SRT 接続
（配信と再生の両方）では、そのパスフレーズによる暗号化が必須になります。パスフレーズのない接続や、パスフレーズが異なる接続は
拒否します（暗号化なしの接続に自動で切り替えることはありません）。この秘密値がないテナントでは、暗号化を使いません。
パスフレーズの長さは、SRT の仕様により 10〜79 文字です。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"srt-passphrase","value":"correct-horse-battery"}'
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY&passphrase=correct-horse-battery"
```

秘密値の作成には `tenant:admin` の権限が必要です。SENCA から外部へ接続する SRT（`srt_pull` 入力・`srt_push` 出力）のパスフレーズは
これとは別で、入力・出力の `credential_ref` で指定します（[6. 入力と出力](06-inputs-outputs.md)）。

## 4.6 WHIP で配信する（WebRTC）

`--webrtc-udp ADDR` を指定すると、WebRTC が有効になります（ICE-lite。1 つの UDP ソケットをすべてのセッションで共有します）。
`0.0.0.0:ポート` で待つと、ICE の候補にはノードのネットワークインターフェースのアドレスを載せます。NAT や Docker の内側のように、
配信者から見えるアドレスが違う場合は、`--webrtc-public-ip` でそのアドレスを指定してください（[3. 設定](03-configuration.md)）。

| 操作 | リクエスト | 応答 |
|---|---|---|
| 開始 | `POST /whip/{stream_id}`、`Authorization: Bearer {stream_key}`（または `publish` 権限の API トークン）、`Content-Type: application/sdp`、本文は offer | 201、本文は answer（`application/sdp`）、`Location: /whip/{stream_id}/{session}` |
| trickle ICE・ICE restart | `PATCH {Location}`、`Content-Type: application/trickle-ice-sdpfrag` | 204、または 200 と sdpfrag（ICE restart） |
| 終了 | `DELETE {Location}` | 200 |

- パスにはストリームの **ID** を使います（名前ではありません）。`ingest.whip_url` をそのまま使えます。
- `Authorization` には、ストリームキーの代わりに、同じテナントの `publish` 権限を持つ API トークン（または JWT）も使えます。
  配信の機材にストリームキーを渡さずに済みます（[12. セキュリティ](12-security.md)）。
- キーやトークンが誤っていれば 401、Content-Type が誤っていれば 415、offer を受け付けられなければ 400、`--webrtc-udp` を指定していないノードでは 404 になります。
- `PATCH`・`DELETE` は、推測できない 64 bit のセッション ID だけで受け付けます（`Location` を第三者に知られないようにしてください）。
- CORS: `OPTIONS` に応答し、`Location`・`ETag` ヘッダーを公開します（ブラウザーから配信できます）。
- 受信できるコーデック: H.264（Constrained Baseline・High、packetization-mode 1）・VP8・VP9 profile 0・AV1・Opus・PCMU・PCMA。
  サイマルキャストの場合は、最初のレイヤーだけを使います。
- HLS のセグメントを区切るため、SENCA は 2 秒ごとに配信者へキーフレームを要求します（PLI）。
- ブラウザーから配信するためのページ（`apps/sender-web`）は、`--sender-dir` を指定すると `/sender/` で配信されます。

```sh
curl -s -X POST "http://media.example.com:8080/whip/$SID" -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/sdp" --data-binary @offer.sdp -D headers.txt -o answer.sdp
```

## 4.7 キャプチャデバイスから取り込む

ノードに接続したカメラやキャプチャカードは、入力（`protocol: "device"`）としてストリームに取り込みます。デバイスの SDK はノードには
読み込みません。別プロセスの `senca-capture-worker`（`senca-node` と同じディレクトリに置きます）が取り込みとエンコードを行い、
エンコード済みの AU だけをノードに渡します。

使用できるデバイスは、`GET /api/v1/devices` の `nodes[].capture` に表示されます（`node:admin` の権限が必要です）。

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{
  "protocol": "device",
  "url": "device:v4l2:/dev/video0",
  "options": {"codec": "vp9", "width": "1280", "height": "720", "fps": "30", "bitrate_bps": "2500000",
              "audio_device": "alsa:hw:1,0", "audio_codec": "opus"}
}'
```

| 項目 | 既定 | 説明 |
|---|---|---|
| `url` | （必須） | `device:{デバイス ID}`（例: `device:v4l2:/dev/video0`・`device:decklink:0`）。300 文字まで |
| `options.codec` | `vp9` | 映像のエンコード形式。H.264/H.265 を選ぶと GPU でのエンコードが必要です（グリッドが GPU のセッション数を管理します） |
| `options.width`・`height`・`fps` | 1280・720・30 | |
| `options.bitrate_bps` | 2500000 | |
| `options.audio_device` | なし | 指定したときだけ音声も取り込みます |
| `options.audio_codec` | `opus` | |

options の値はすべて文字列で指定します。ストリームが `royalty_free` の場合は、取り込みの worker もその方針に従ってエンコード形式を選びます。
キャプチャの入力は、そのデバイスを持つノードに配置されます（グリッドのワークロード。[9. クラスタとグリッド](09-cluster-grid.md)）。
入力の状態と再接続の動作は、[6. 入力と出力](06-inputs-outputs.md) と同じです。`--capture-synthetic` を指定すると、
テスト用の合成デバイス（`device:synthetic:0`）を使えます。

## 4.8 再生 URL（署名付き）

再生には `/api/v1` 以外の URL を使い、Authorization ヘッダーは使いません。URL に有効期限と HMAC 署名が含まれます。

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

- リクエストの本文は省略できます。`ttl_secs` の既定は 300 で、範囲は 1〜`--max-playback-ttl`（既定 86400）です。必要な権限は `subscribe` です。
- `rtsp` は `--public-rtsp-url` を指定したときだけ出力されます。`whep` は、`--webrtc-udp` を指定していないノードでは `null` になります。
  `moq`（MoQ の WebTransport の URL。[4.15](#415-moq-と-http3) を参照）は、`--quic` を指定したノードでだけ出力されます。
- 署名は `live/{stream_id}/{exp}` に対する HMAC-SHA256 です（先頭 16 バイト、base64url）。ファイル名は署名の対象に含まれないため、
  同じ `/s/{exp}/{sig}/live/{id}/` の下にある任意のファイル（セグメント・`master.m3u8`・字幕）に使えます。
- 有効期限切れや署名の誤りは 403 になります。署名の鍵は制御状態に含まれ、クラスターの全ノードで共通です（どのノードでも検証できます）。
- 発行済みの URL は、有効期限まで取り消せません。`ttl_secs` は短くしてください（[12. セキュリティ](12-security.md)）。

## 4.9 HLS

| パス | 内容 |
|---|---|
| `/s/{exp}/{sig}/live/{id}/index.m3u8` | ライブのプレイリスト |
| `…/{seq}.ts`・`…/{seq}.m4s` | セグメント（`hls.container` によって決まります） |
| `…/{seq}.{part}.m4s`（`.ts`） | LL-HLS の part |
| `…/init{n}.mp4` | fMP4 の初期化セグメント（設定が変わるたびに番号が増えます） |
| `…/master.m3u8`・`sub_{lang}.m3u8`・`sub_{lang}_{n}.vtt` | 字幕がある場合（[8. 字幕](08-captions.md)） |

- セグメントはメモリから返し、ディスクは使いません。プレイリストの範囲から外れたセグメントも、しばらく（4 本分）は取得できます。
- プレイリストには `Cache-Control: no-cache`、セグメントには `max-age=60` が付きます。応答には `Access-Control-Allow-Origin: *` が付きます。
- セグメントの番号は、配信者が再接続しても続きから増えます（プレイリストの `EXT-X-MEDIA-SEQUENCE` が戻ることはありません）。
- 配信が終了する（再接続の猶予時間が過ぎる）と、プレイリストに `#EXT-X-ENDLIST` が付きます。
- コンテナに格納できないコーデックは、HLS には出力されません。TS: H.264・H.265・AAC・Opus。fMP4: H.264・H.265・AV1・VP9・AAC・Opus。
  `POST /streams/{id}/plan` を使うと、現在の入力が各出力でどう扱われるか（remux・unsupported とその理由）を確認できます。

**LL-HLS**（`latency_profile: "low"`）: part の目標の長さは `min(500 ms, segment_ms / 4)`（200 ms 以上）です。プレイリストには
`EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=…,PART-HOLD-BACK=…`・`EXT-X-PART-INF`・`EXT-X-PART`・
`EXT-X-PRELOAD-HINT` が出力されます。

| クエリ | 動作 |
|---|---|
| `_HLS_msn=N` | セグメント N が完成するまで待ってから返します（ブロッキングリロード） |
| `_HLS_msn=N&_HLS_part=P` | セグメント N の part P ができるまで待ちます |
| `_HLS_skip=YES`（または `v2`） | 差分のプレイリストを返します（古いセグメントを `EXT-X-SKIP` で省略） |

待ち時間は最長 6 秒で、間に合わなければ 503（`Retry-After: 1`）を返します。作成中のセグメントより 2 つ以上先の番号、100 以上の `_HLS_part`、
数値でない値を指定すると 400 になります。配信が終了している場合は、すぐに返します。

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

## 4.10 試してみる: RTMP で配信して HLS で再生する

次の手順は、ローカルのノードで動作を確認しています。

```sh
# 1. ストリームを作成する
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"cam1"}' > s.json
SID=$(python3 -c "import json;print(json.load(open('s.json'))['id'])")
KEY=$(python3 -c "import json;print(json.load(open('s.json'))['ingest']['stream_key'])")

# 2. テスト信号を RTMP で配信する（別の端末で実行）
ffmpeg -re -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -g 60 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "rtmp://127.0.0.1:1935/live/$KEY"

# 3. 再生 URL を取得して再生する
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
curl -s "$HLS"            # #EXTM3U … 0.ts 1.ts …
ffplay "$HLS"             # または ffprobe / ブラウザーの hls.js
```

`-g 60`（30 fps で 2 秒）のように、キーフレームの間隔をセグメントの長さに合わせてください。セグメントはキーフレームの位置でしか区切れません。

## 4.11 RTMP play

署名付き URL の `rtmp`（`rtmp://{host}/play/{stream_id}?exp=..&sig=..`）をそのまま使います。application は `play`、
stream name は `{stream_id}?exp=..&sig=..` です。署名の誤りや有効期限切れは `NetStream.Play.StreamNotFound` になります。

```sh
ffplay "rtmp://127.0.0.1:1935/play/$SID?exp=$EXP&sig=$SIG"
```

送信形式は FLV（Enhanced RTMP を含む）です。受信の遅い視聴者は、送信待ちのデータが 8 MiB を超えると、次のキーフレームまでデータを破棄します。

## 4.12 SRT play

Stream ID は `play:{stream_id}?exp={exp}&sig={sig}`（署名は再生 URL と同じ）、または `#!::r={stream_id}?exp=..&sig=..,m=request` です。
送信するデータは MPEG-TS（1316 バイト単位）です。テナントに `srt-passphrase` がある場合は、パスフレーズが必要です。

URL の `streamid=` に指定するときは、値の中の `?`・`&`・`=` をパーセントエンコードしてください（エンコードしないと `&` で値が区切られ、
署名が届かずに拒否されます）。

```sh
ffplay "srt://127.0.0.1:9000?streamid=play:$SID%3Fexp%3D$EXP%26sig%3D$SIG"
```

`EXP`・`SIG` は、再生 URL の `rtmp` などのクエリから取り出します。TS に格納できないコーデック（AV1・VP8・VP9）は、この方法では再生できません。

## 4.13 RTSP play

`--rtsp ADDR` で RTSP の待ち受けを開始し、`--public-rtsp-url` を設定すると、再生 URL に `rtsp` が出力されます。

- URL: `rtsp://{host}:{port}/live/{stream_id}?exp={exp}&sig={sig}`
- 転送方式は **TCP interleaved のみ**です。UDP の SETUP は 461 で拒否します。プレイヤーでは TCP を指定してください（ffmpeg では `-rtsp_transport tcp`）。
- 署名が誤っている場合は DESCRIBE が 404 に、配信者がいないか RTP で送れるトラックがない場合は 415 になります。

```sh
ffplay -rtsp_transport tcp "rtsp://127.0.0.1:8554/live/$SID?exp=$EXP&sig=$SIG"
```

## 4.14 WHEP

`POST {whep の URL}`（`Content-Type: application/sdp`、本文は offer）を送ると、201 と answer、`Location: /whep/{stream_id}/{session}` が返ります。
`PATCH`・`DELETE` は WHIP と同じです。

- SDP で提示するのは、ストリームが現在持っているコーデックだけです（H.264・VP8・VP9・AV1・Opus・PCMA・PCMU）。
- **AAC の音声は WebRTC で送れないため、出力しません**（特許への対応方針により、AAC のデコードは行いません。WHEP で音声も必要な場合は、
  配信者側で Opus を使ってください）。送れるトラックが 1 つもない場合は 422 になります。
- 署名の誤りや有効期限切れは 403 になります。
- 接続が確立するまでの間は直近の GOP を保持しておき、接続後に送信します（キーフレームから始まります）。

## 4.15 MoQ と HTTP/3

`--quic ADDR`（と `--tls-cert`・`--tls-key`）を指定すると、1 つの UDP ポートで QUIC v1 を受け付けます。既定では無効です。
接続ごとに、ALPN によってプロトコルが決まります。

| ALPN | プロトコル |
|---|---|
| `h3` | HTTP/3。TCP の HTTP と同じルートを提供します（`/api/v1`、LL-HLS のブロッキングリロードを含む署名付き HLS、WHIP/WHEP、`/healthz` など）。SSE（`/api/v1/events`）と `/signaling`（WebSocket）は 501 を返すので、HTTP/1.1 を使ってください。同じ接続で `/moq` への拡張 CONNECT を送ると、WebTransport の MoQ になります |
| `moqt-22` | ネイティブ QUIC 上の MoQ（draft-ietf-moq-transport-22） |

- `--quic` を指定すると、HTTP・HTTPS の応答に `Alt-Svc: h3=":ポート"; ma=86400` が付き、ブラウザーは次のリクエストから HTTP/3 を使えます。
- 証明書には、ノードの `--tls-cert`・`--tls-key`（TCP の HTTPS と同じもの）を使います。
- MoQ は Internet-Draft の段階の仕様のため、バージョンを draft-22 に固定しています。他のバージョンを使うクライアントは、
  ALPN または `wt-available-protocols` のネゴシエーションの段階で拒否します。

### 購読（MoQ で視聴する）

ライブ中のストリームは、すべて namespace `senca/live/{stream_id}` として購読できます（別のノードが所有するストリームは、
中継のミラーから配信します）。

| track | 内容 |
|---|---|
| `catalog.json` | hang 形式の catalog（moq-dev/moq と同じ形式）。更新ごとに 1 group・1 object です。`video`・`audio` の `renditions` に `codec`（RFC 6381）、`description`（avcC などの 16 進）、`container: {"kind":"cmaf","init": base64(ftyp+moov)}` が入ります |
| `video`・`audio`（2 本目以降は `video2` など） | CMAF。1 object が 1 fragment（moof+mdat、1 サンプル）です。映像は GOP ごと、音声はおよそ 1 秒ごとに新しい group になります |

- 認可には再生 URL と同じ署名（`exp`・`sig`）を使います。次のいずれかで渡してください。
  - MoQ の AUTHORIZATION TOKEN（SETUP または SUBSCRIBE）の値として `exp=..&sig=..`
  - ネイティブ QUIC の PATH（`moqt://host:port/?exp=..&sig=..`）
  - WebTransport の URL のクエリ
- 署名がない、誤っている、または有効期限が切れている SUBSCRIBE・FETCH・TRACK_STATUS は、`UNAUTHORIZED` で拒否します。
  SUBSCRIBE_NAMESPACE には、署名が有効なストリームだけを通知します。
- `--quic` を指定したノードでは、再生 URL（4.8）の `moq` に WebTransport の URL（`https://{host}:{port}/moq?exp=..&sig=..`）が
  入ります。host は `--public-http-url` の host です。
- 途中から購読した場合は、現在の group の先頭（キーフレーム）から配信します。draft-22 の FILL_PARAMETERS
  （current-group join）が指定されている場合は、group の先頭部分を fill の stream で、その続きを購読で配信します。
  直近の 2 group は FETCH でも取得できます。

moq-dev/moq の CLI で fMP4 に書き出す例:

```sh
Q="exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw"   # 再生 URL の moq のクエリ
moq --connect "moqt://media.example.com:4433/?$Q" --connect-version moq-transport-22 \
  --broadcast "senca/live/$SID" export fmp4 > live.mp4
```

### 配信（MoQ で送信する）

ストリームキー（`sk_...`）を AUTHORIZATION TOKEN（SETUP または PUBLISH_NAMESPACE）に入れて
`PUBLISH_NAMESPACE senca/live/{stream_id}` を送ると、ノードが `catalog.json` と catalog に記載された track を購読し、
そのストリームの配信者になります（`live.transport` は `moq` です）。

- 受け付けるコンテナは `cmaf`（init と fragment）だけです。hang の `legacy`・`loc` の track が含まれている場合は、拒否してセッションを閉じます
  （moq-dev の `moq import` は `legacy` を出力するため、そのままでは配信できません）。
- 配信者側から始める PUBLISH には対応していません（`NOT_SUPPORTED` を返します）。1 つのセッションで配信できるストリームは 1 つです。
- 予約の「live」の時間帯の外では、他の配信方法と同じく拒否します。

## 4.16 静止画

| 操作 | リクエスト | 権限 | 結果 |
|---|---|---|---|
| 現在の画像 | `GET /streams/{id}/snapshot[?format=jpeg\|png]` | `stream:read` | 200 `image/jpeg`（既定）または `image/png` |
| 保存済みの一覧 | `GET /streams/{id}/snapshots` | `stream:read` | `{"items":[{id, taken_at, reason, width, height, bytes}]}`（古い順） |
| 1 枚の取得 | `GET /streams/{id}/snapshots/{snapshot_id}` | `stream:read` | JPEG |
| 削除 | `DELETE /streams/{id}/snapshots/{snapshot_id}` | `stream:write` | 204 |

- 静止画は、直近の GOP を**デコード**して作成します。デコードは codec worker で行います。H.264・H.265 は、GPU のデコーダーがないと 503
  `CAPACITY_UNAVAILABLE`（`retryable: false`）になります。AV1（dav1d）・VP8/VP9（libvpx）はソフトウェアでデコードできます
  （[14. コーデックと特許](14-codecs-and-patents.md)）。配信中でない場合は 409 になります。
- キーフレームがまだ届いていない場合は、最大 5 秒待ちます。届かなければ 504（`INPUT_TIMEOUT`、再試行できます）になります。
- クラスターで、配信を別のノードが受けている場合は 409（`OWNERSHIP_LOST`）になります。`details.owner_api` のノードに頼み直してください。
- E2EE の部屋から書き出したストリーム（[11. メディアパイプライン](11-media-pipelines.md)）は、中身を見られないため 422 になります。
- 静止画は次の 3 つの理由（`reason`）で保存されます: `interval`（ストリームの `snapshots` の設定。`keep` 枚を超えると古いものから削除）、
  `schedule`（`snapshot` の予約。[5. 録画](05-recording.md)）、`board_change`（パイプラインの `snapshot_on_change`。[7. パイプライン](07-pipelines.md)）。
- 保存先は `{data-dir}/snapshots/{stream_id}/` です。ストリームを削除すると、静止画も一緒に削除されます。
- 録画のサムネイルについては [5. 録画](05-recording.md) を参照してください。

```sh
curl -s -o now.jpg "$API/streams/$SID/snapshot" -H "Authorization: Bearer $TOKEN"
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" -d '{"snapshots":{"interval_secs":60,"keep":1440}}'
```

## 4.17 配信者の接続が切れたとき・入れ替わったとき

- 配信者の接続が切れても、`--reconnect-grace`（既定 30000 ms）の間は配信を終了しません。その間に同じキーで再接続すると、
  `epoch` を 1 増やして同じ配信として続けます。HLS は `EXT-X-DISCONTINUITY` で区切り、セグメントの番号は続きから増えます。
  録画は 1 本のまま続きます（[5. 録画](05-recording.md)）。再接続時のプロトコルは異なってもかまいません（RTMP で切れて SRT で再接続するなど）。
- 猶予時間が過ぎると、配信は終了します。HLS に `ENDLIST` が付き、RTMP/SRT/RTSP/WHEP の視聴者の接続は閉じられ、録画は確定します。
- `desired_state: stopped` にすると、配信者からの受信をすぐに止め、猶予時間を待たずに配信を終了します。

## 4.18 どのノードからでも再生できる（クラスター）

クラスターでは、配信者からの接続を受けたノードが、そのストリームの所有者になります。所有者でないノードに再生のリクエスト
（HLS・RTMP/SRT/RTSP play・WHEP）が届くと、そのノードは所有者から中継で AU を受け取ってローカルにミラーを作り、そこから配信します。
同じストリームの視聴者が何人いても、上流からの転送は 1 本です。最初の HLS のリクエストでは、ミラーの最初のプレイリストができるまで
最長 6 秒待ちます。使われなくなったミラーは 30 秒後に削除されます。ミラーでは録画と字幕は行いません（これらは所有者のノードが行います）。
中継が使えない場合は、所有者のノード（`media_addr`）へ 307 でリダイレクトします。詳しくは [9. クラスタとグリッド](09-cluster-grid.md) を参照してください。

## 4.19 よくあるトラブル

| 症状 | 原因 |
|---|---|
| RTMP の配信が拒否される | `BadName`: キーの誤り、すでに別の配信者がいる、`stopped`。`Rejected`: `live` 予約の時間帯の外 |
| SRT の接続がすぐに拒否される | Stream ID の形式の誤り、キーの誤り、テナントに `srt-passphrase` があるのにパスフレーズを指定していない（ログに `unsecure` と出力） |
| SRT play が拒否される | `streamid` の値をパーセントエンコードしていない |
| HLS が 404 になる | 最初のセグメントがまだできていない（キーフレームの間隔が長い）、URL のホストが誤っている（`--public-http-url`） |
| HLS に映像・音声が出ない | コーデックがコンテナに格納できない（`POST /streams/{id}/plan` で確認）、`royalty_free` により H.264/AAC が破棄された |
| 静止画が 503 になる | H.264/H.265 で GPU がない、codec worker が起動していない |
| WHEP で音声が出ない | 音声が AAC である |
