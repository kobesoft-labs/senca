# 6. 入力と出力

[English](../en/06-inputs-outputs.md)

SENCA から外へつないで映像を取りに行く**入力**（pull）と、stream を外へ送る**出力**（push・PaaS 連携）。
送り手が SENCA につなぐ push（RTMP・SRT・WHIP）は stream の鍵で受けるので、ここではなく [4. 取り込みと再生](04-ingest-playback.md) で扱う。

- 設計の理由: [ADR-0007 入力・出力と外向きの接続](../../dev/adr/0007-inputs-outputs-egress.md)、[ADR-0010 PaaS 連携](../../dev/adr/0010-paas.md)、[ADR-0013 グリッド](../../dev/adr/0013-grid.md)
- 関係するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[15. API と SDK](15-api-and-sdks.md)
- 実装: `crates/senca-node/src/io.rs`・`hls_pull.rs`・`rtsp_pull.rs`・`srt.rs`・`capture.rs`・`paas.rs`・`egress.rs`・`api/io.rs`、`crates/senca-paas`

例の `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じ。

## 6.1 API

| 操作 | 要求 | 権限 |
|---|---|---|
| 入力を足す | `POST /streams/{id}/inputs` | `stream:write` |
| 入力の一覧 | `GET /streams/{id}/inputs?limit&cursor` | `stream:read` |
| 入力の取得・変更・削除 | `GET`・`PATCH`・`DELETE /inputs/{id}` | `stream:read`・`stream:write` |
| 出力を足す | `POST /streams/{id}/outputs` | `stream:write` |
| 出力の一覧 | `GET /streams/{id}/outputs?limit&cursor` | `stream:read` |
| 出力の取得・変更・削除 | `GET`・`PATCH`・`DELETE /outputs/{id}` | `stream:read`・`stream:write` |
| 秘密値 | `POST`・`GET /secrets`、`DELETE /secrets/{id}` | `tenant:admin` |

本文:

| フィールド | 入力 | 出力 | 意味 |
|---|---|---|---|
| `protocol` | 必須 | 必須 | 入力: `rtmp_pull`・`rtsp_pull`・`srt_pull`・`hls_pull`・`device`。出力: `rtmp_push`・`srt_push`・`paas` |
| `url` | 必須 | `paas` 以外で必須 | 接続先。スキームは protocol で決まる（下表）。2048 文字まで、空白・制御文字は不可、**`user:pass@` を入れると 400** |
| `credential_ref` | 任意 | 任意（`paas` は必須） | `secret:{name}`。資格情報は URL ではなくこれで渡す（§6.6） |
| `enabled` | 既定 `true` | 既定 `true` | 偽なら動かさない |
| `options` | 任意 | 任意 | 文字列の表。16 個まで、キー 64 文字・値 256 文字まで |
| `provider` | | `paas` で必須 | `cloudflare_stream`・`aws_ivs`・`aws_medialive` |
| `placement` | 任意 | 任意 | 置き場所の指定（§6.8） |

| protocol | `url` のスキーム |
|---|---|
| `rtmp_pull`・`rtmp_push` | `rtmp`・`rtmps` |
| `rtsp_pull` | `rtsp`・`rtsps`（TLS でつなぐ） |
| `srt_pull`・`srt_push` | `srt` |
| `hls_pull` | `http`・`https` |
| `device` | `device:{機器 ID}`（[4.7](04-ingest-playback.md)） |

- 1 つの stream の入力は 4 つまで、出力は 16 個まで（超えると 403 `QUOTA_EXCEEDED`）。
- `PATCH` で変えられるのは `url`・`credential_ref`（`null` で外す）・`enabled`・`options`・`placement`。`If-Match` 必須。
  `protocol`・`provider` は変えられない。変えると世代が上がり、動いている接続を作り直す。
- 応答には `status`（§6.5）と `placement`・`placement_status`（§6.8）が付く。出力にはさらに `provider`・`provider_resource`。

## 6.2 入力（pull）

入力は stream の送り手として振る舞う。**stream の送り手は同時に 1 つ**なので、push の送り手がつながっている間は入力は
つなげず、つなぎ直しを繰り返す（逆も同じ）。入力の切断も、送り手の切断と同じく再接続の猶予（[4.16](04-ingest-playback.md)）の対象になる。

### rtmp_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtmp_pull","url":"rtmp://origin.example.com/live/program1"}'
```

- `url` は `rtmp[s]://host[:port]/{app}/{stream name}`。ポートの既定は `rtmp` が 1935、`rtmps` が 443。
- stream name は `credential_ref` の秘密値があればそれを、無ければ URL の最後の部分を使う（どちらも無ければエラー）。
- ライブとして再生する（play の start = -2）。20 秒受信が止まると切ってつなぎ直す。

### rtsp_pull

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtsp_pull","url":"rtsp://192.0.2.10:554/stream1","credential_ref":"secret:cam-login"}'
```

- 転送は **TCP interleaved だけ**（NAT・ファイアウォールを越えやすい）。ポートの既定は 554。
- 資格情報は秘密値に `user:password` の形で入れる。Basic 認証は `options.allow_basic: "true"` のときだけ使う（平文でパスワードを送るため）。
- `rtsps://` は TLS でつなぐ（ポートの既定は 322）。サーバーの証明書は OS が信頼する認証局で検証する（ノードの環境変数 `SSL_CERT_FILE`
  で認証局のファイルを指定することもできる）。自己署名の証明書を使うカメラは、`options.tls_spki_sha256` に証明書の公開鍵（SPKI）の
  SHA-256 を base64 か 16 進で書いて固定する（`,` で区切って複数書ける）。固定したときは、認証局・期限・ホスト名は確かめない。
  固定の値が合わなければ TLS で失敗し、平文には切り替えない。値の形が違えば 400。

  ```sh
  # カメラの証明書から固定の値を作る
  openssl x509 -in cam.crt -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
  curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" \
    -d '{"protocol":"rtsp_pull","url":"rtsps://192.0.2.10:322/stream1","credential_ref":"secret:cam-login","options":{"tls_spki_sha256":"<上の値>"}}'
  ```
- 社内のカメラなど私設アドレスへつなぐには `--egress-allow` が要る（§6.7）。

### srt_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"srt_pull","url":"srt://contrib.example.com:9000?streamid=feed1&latency=200000","credential_ref":"secret:srt-feed-pass"}'
```

- SENCA が caller になってつなぐ。中身は MPEG-TS。
- `url` のクエリで読むのは `streamid` と `latency` だけ。**`latency` はマイクロ秒**（ffmpeg の URL と同じ。`200000` = 200 ms）。既定は 200 ms。
- パスフレーズは URL に入れず、`credential_ref` で渡す（10〜79 文字）。

### hls_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"hls_pull","url":"https://cdn.example.com/live/master.m3u8"}'
```

- multivariant playlist なら帯域（`BANDWIDTH`）の最も大きい variant を選ぶ。
- TS と fMP4 のセグメントに対応。セグメントを実時間の速さで送り直す。
- ライブの再生リストは、最初は末尾の 3 セグメントから読み、以後は `TARGETDURATION` の半分（0.5〜5 秒）ごとに読み直す。
- `#EXT-X-ENDLIST` のある再生リスト（VOD）は最後まで読んだら終わり、状態は `ended` になる（繰り返さない）。
- 入力のセグメントをためる分の遅延は消せない。

### device

キャプチャ機器。[4.7 キャプチャ機器から入れる](04-ingest-playback.md) を参照。

## 6.3 出力（push）

出力は stream の配信が始まるまで `waiting_input` で待ち、始まったら相手へつなぐ。配信が終わると接続を閉じ、次の配信を待つ。

### rtmp_push

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"yt-key","value":"xxxx-xxxx-xxxx-xxxx"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"rtmp_push","url":"rtmps://live.example.com:443/app","credential_ref":"secret:yt-key"}'
```

- `url` は `rtmp[s]://host[:port]/{app}[/{stream key}]`。stream key は `credential_ref` があればそれを、無ければ URL の最後の部分を使う。
  鍵を URL に書くと API の応答（`url`）に見えるので、`credential_ref` を勧める。
- FLV（Enhanced RTMP を含む）で送る。stream のコーデックを相手が受けられるかは相手次第。
- 接続・publish の開始に 15 秒、書き込みに 10 秒の時間切れ。

### srt_push

```sh
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"srt_push","url":"srt://receiver.example.com:9000?streamid=senca&latency=300000","credential_ref":"secret:srt-out-pass"}'
```

- caller でつなぎ、MPEG-TS を 1316 バイトずつ送る。`streamid`・`latency`（マイクロ秒）は srt_pull と同じ。パスフレーズは `credential_ref`。
- TS に入らないコーデック（AV1・VP8・VP9）は送れない。

`whip_push`（外部の WHIP サーバーへ送る）は今のノードには無い。`protocol: "whip_push"` は 400。

## 6.4 PaaS 連携（`protocol: "paas"`）

クラウドのメディアサービスに**受け口を SENCA が作り**、そこへ SENCA 自身の RTMP(S)・SRT で送る。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cf","value":"0123456789abcdef0123456789abcdef:CF_API_TOKEN"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf","options":{"recording":"automatic"}}'
```

動き:

1. 出力が有効になると状態は `provisioning`。外部の資源（Live Input・Channel・Input）を作る。資源には `senca:owner={出力の ID}`
   のタグ（Cloudflare は `meta`）を付け、作る途中の経過と資源の ID を出力の `provider_resource` に保存してから次の要求を送る
   （途中で落ちても二重に作らない。同じ owner の資源があれば引き取る）。
2. 受け口の秘密（stream key・パスフレーズ）は保存しない。送るたびに外部の API から取り直す。
3. 受け口へ `rtmp_push`・`srt_push` と同じ方法で送る。受け口の種類は `options.ingest` で選べる（`rtmps` か `srt`。無ければ連携先の最初の受け口）。
4. **出力を消すと外部の資源も消す。** SENCA のタグの無い資源は消さない。後片付けに失敗しても出力の定義は消え、失敗はログに残る。

外部の API への接続も §6.7 の宛先の制限を受ける。

| `provider` | 作るもの | `credential_ref` の値 | `options` |
|---|---|---|---|
| `cloudflare_stream` | Cloudflare Stream の Live Input | `account_id:api_token` | `recording`: `off`（既定）・`automatic`。`customer_subdomain`: `customer-xxxx.cloudflarestream.com`。`endpoint`: API の接続先の上書き（試験用） |
| `aws_ivs` | Amazon IVS の Channel | `access_key_id:secret_access_key[:session_token]` | `region`（必須）。`channel_type`: `STANDARD`（既定）・`BASIC`・`ADVANCED_SD`・`ADVANCED_HD`。`latency_mode`: `LOW`（既定）・`NORMAL`。`recording_configuration_arn`。`endpoint` |
| `aws_medialive` | AWS Elemental MediaLive の Input と Input Security Group | `access_key_id:secret_access_key[:session_token]` | `region`（必須）。`input_type`: `RTMP_PUSH`（既定）・`SRT_LISTENER`。`allow_cidr`: ノードの送出元の CIDR（`,` 区切り、10 個まで）か `input_security_group`（既存の ID。SENCA は作らず消さない）のどちらかが必須。`pipelines`: `1`（既定）・`2`。`rtmp_app`（既定 `senca`）。`srt_passphrase_secret_arn`。`srt_min_latency_ms`（120〜15000、既定 2000）。`endpoint` |

- Cloudflare: 送り先は RTMPS（既定）か SRT。
- IVS: 送り先は `rtmps://{ingestEndpoint}:443/app/`。Channel に SRT があれば SRT も選べる。配信中の Channel は連携先が削除を拒む（`ConflictException`）。後片付けがこれで失敗したらログに残るので、手で消す。
- MediaLive: 作るのは SENCA が送り込む Input と、それを守る Security Group だけ。**Channel（エンコード設定・出力先）は利用者が別に作り**、
  この Input をつなぐ。Channel につながったままの Input は消せない。`provider_resource` は `input=<id>;sg=<id>`。

## 6.5 状態とつなぎ直し

入力・出力の `status`:

| フィールド | 意味 |
|---|---|
| `state` | `starting`・`provisioning`（PaaS）・`waiting_input`（出力が配信を待つ）・`connecting`・`active`・`backoff`・`error`・`ended`（VOD の `hls_pull` を読み終えた） |
| `last_error` | 最後の失敗の理由（秘密は含まない） |
| `connects` | つながった回数 |
| `since` | 今の `state` になった時刻 |
| `bytes` | 送受信した量 |

`status` は、その入力・出力を動かしているノードで取得したときに値が入る（他のノード・動いていないときは `null`）。

失敗すると `error` になり、すぐ `backoff` に移って指数バックオフで待ってからつなぎ直す。待ち時間は 1 秒から倍々で 60 秒まで、
各回の実際の待ちはその半分〜全部の間でばらつく。正常に終わった（相手が配信を終えた）ときは短い待ち（0.5〜1 秒）で次を始める。

実行例（`--egress-allow` 無しで 127.0.0.1 の RTSP カメラを指定）:

```json
{"state": "backoff", "last_error": "127.0.0.1 は許されていない宛先です（--egress-allow で許可が必要）", "connects": 0, "bytes": 0,
 "since": "2026-11-03T01:19:43.679Z"}
```

## 6.6 秘密値（`credential_ref`）

資格情報は秘密値として保存し、`secret:{name}` で参照する。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
```

```json
{"id":"33bb760ab08f2f11af9fcd12af258649","name":"cam-login","ref":"secret:cam-login","created_at":"2026-11-03T01:19:07.291Z"}
```

- `name` は stream の名前と同じ規則（1〜64 文字の英数字と `-_.`）、テナント内で一意（重複は 409）。`value` は 1〜8192 バイト。
- 値は AES-256-GCM で暗号化して保存します（単一ノードではノードの鍵 `{data-dir}/control/secret-key`、クラスタではクラスタ共通の鍵を使います。[12. セキュリティ](12-security.md)）。**値は API から二度と読み出せない**
  （一覧は `id`・`name`・`ref`・`created_at` だけ）。値を変えるには消して作り直す。
- テナントの秘密値は 500 個まで。
- 入力・出力・stream の字幕が参照している秘密値は消せない（409）。
- 存在しない秘密値を参照すると、入力・出力の作成・変更が 400。

| 使う所 | 値の形 |
|---|---|
| `rtmp_pull` | stream name |
| `rtmp_push` | stream key |
| `rtsp_pull` | `user:password` |
| `srt_pull`・`srt_push` | パスフレーズ（10〜79 文字） |
| `paas` | §6.4 の表 |
| 字幕（[8. 字幕](08-captions.md)） | API キー |
| 名前 `srt-passphrase` | SRT の受け口のパスフレーズ（[4.5](04-ingest-playback.md)） |

## 6.7 外向きの接続の制限（egress）

pull 入力・push 出力・PaaS の API・字幕の API がつなぐ先は、名前を解決した**後の IP** で検査する（DNS で私設アドレスに向けられても拒否する）。
既定で拒否するもの:

- ループバック（127.0.0.0/8、::1）、未指定、マルチキャスト、ブロードキャスト、0.0.0.0/8
- リンクローカル（169.254.0.0/16。クラウドのメタデータ 169.254.169.254 を含む、fe80::/10）
- 私設アドレス（10/8、172.16/12、192.168/16、100.64/10、fc00::/7）。IPv4 射影の IPv6 も同じ

社内のカメラ・送り先へつなぐときは、管理者が `--egress-allow CIDR` で範囲を許す（何度でも書ける）。

```sh
senca-node ... --egress-allow 192.0.2.0/24 --egress-allow 10.20.0.0/16
```

許されない宛先は、入力・出力の作成時ではなく**接続時**に失敗し、`last_error` に理由が出る（§6.5 の例）。

## 6.8 置き場所（グリッド）

`enabled: true` の入力・出力はグリッドの仕事（`input:{id}`・`output:{id}`）になり、配置係が割り当てたノードだけが動かす。

- 出力は、その stream の配信を受けているノードを好む（中継の 1 段を省く）。
- キャプチャの入力は機器を持つノードに置かれる。H.264/H.265 で符号化するキャプチャは GPU の符号化のセッションを予約する。
- `placement` で手動の割り当て（`node`・`device`）、ラベル、反親和性、優先度、置き方（`spread`・`binpack`）、割り当て先が止まったときの
  扱い（`pin_policy`）を指定できる。`degrade` はパイプラインだけで、入力・出力に書くと 400。
- 結果は `placement_status`（`state`: `running`・`pending`・`unschedulable`・`not_scheduled`、`node`・`node_name`・`reason` など）。

詳しくは [9. クラスタとグリッド](09-cluster-grid.md)。単一ノードでも同じ仕組みで動く（置き場所は自分だけ）。
