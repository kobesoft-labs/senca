# 6. 入力と出力

[English](../en/06-inputs-outputs.md)

この章では、SENCA と外部のサーバーをつなぐ 2 つの機能を説明します。

- **入力**（pull）: SENCA から外部のサーバーやカメラへ接続し、映像・音声を取り込みます。
- **出力**（push・PaaS 連携）: SENCA のストリームを外部の配信先へ送信します。

配信者（送信側）が SENCA へ接続して送る push（RTMP・SRT・WHIP）は、ストリームキーで受け付けます。
これはこの章ではなく [4. 取り込みと再生](04-ingest-playback.md) で扱います。

- 設計の背景: [ADR-0007 入力・出力と外向きの接続](../../dev/adr/0007-inputs-outputs-egress.md)、[ADR-0010 PaaS 連携](../../dev/adr/0010-paas.md)、[ADR-0013 グリッド](../../dev/adr/0013-grid.md)
- 関連するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスターとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[15. API と SDK](15-api-and-sdks.md)

例で使う `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じです。

## 6.1 API

| 操作 | リクエスト | 権限 |
|---|---|---|
| 入力を追加 | `POST /streams/{id}/inputs` | `stream:write` |
| 入力の一覧 | `GET /streams/{id}/inputs?limit&cursor` | `stream:read` |
| 入力の取得・変更・削除 | `GET`・`PATCH`・`DELETE /inputs/{id}` | `stream:read`・`stream:write` |
| 出力を追加 | `POST /streams/{id}/outputs` | `stream:write` |
| 出力の一覧 | `GET /streams/{id}/outputs?limit&cursor` | `stream:read` |
| 出力の取得・変更・削除 | `GET`・`PATCH`・`DELETE /outputs/{id}` | `stream:read`・`stream:write` |
| シークレット | `POST`・`GET /secrets`、`DELETE /secrets/{id}` | `tenant:admin` |

リクエストの本文:

| フィールド | 入力 | 出力 | 意味 |
|---|---|---|---|
| `protocol` | 必須 | 必須 | 入力: `rtmp_pull`・`rtsp_pull`・`srt_pull`・`hls_pull`・`device`。出力: `rtmp_push`・`srt_push`・`paas` |
| `url` | 必須 | `paas` 以外は必須 | 接続先。スキームは `protocol` で決まります（下の表）。2048 文字まで。空白・制御文字は使えません。**`user:pass@` を含めると 400** になります |
| `credential_ref` | 任意 | 任意（`paas` は必須） | `secret:{name}`。資格情報は URL に書かず、ここで渡します（§6.6） |
| `enabled` | 既定 `true` | 既定 `true` | `false` にすると動作しません |
| `options` | 任意 | 任意 | 文字列のマップ。16 個まで、キーは 64 文字・値は 256 文字まで |
| `provider` | | `paas` では必須 | `cloudflare_stream`・`aws_ivs`・`aws_medialive` |
| `placement` | 任意 | 任意 | 配置先の指定（§6.8） |

| protocol | `url` のスキーム |
|---|---|
| `rtmp_pull`・`rtmp_push` | `rtmp`・`rtmps` |
| `rtsp_pull` | `rtsp`（`rtsps` も受け付けますが、現在は TLS を使わずに接続します） |
| `srt_pull`・`srt_push` | `srt` |
| `hls_pull` | `http`・`https` |
| `device` | `device:{デバイス ID}`（[4.7](04-ingest-playback.md)） |

- 1 つのストリームに設定できる入力は 4 個まで、出力は 16 個までです（超えると 403 `QUOTA_EXCEEDED`）。
- `PATCH` で変更できるのは `url`・`credential_ref`（`null` で解除）・`enabled`・`options`・`placement` です。`If-Match` が必須です。
  `protocol`・`provider` は変更できません。変更すると世代が上がり、動作中の接続を張り直します。
- 応答には `status`（§6.5）、`placement`・`placement_status`（§6.8）が含まれます。出力にはさらに `provider`・`provider_resource` が含まれます。

## 6.2 入力（pull）

入力は、ストリームの配信者として振る舞います。**1 つのストリームの配信者は同時に 1 つだけ**です。
そのため、push の配信者が接続している間は入力は接続できず、再接続を繰り返します（逆の場合も同じです）。
入力が切断されたときも、通常の配信者と同じく再接続の猶予（[4.16](04-ingest-playback.md)）が適用されます。

### rtmp_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtmp_pull","url":"rtmp://origin.example.com/live/program1"}'
```

- `url` の形式は `rtmp[s]://host[:port]/{app}/{stream name}` です。ポートの既定は `rtmp` が 1935、`rtmps` が 443 です。
- stream name には、`credential_ref` のシークレットがあればその値を、無ければ URL の最後の部分を使います（どちらも無ければエラー）。
- ライブとして再生します（play の start = -2）。受信が 20 秒止まると、接続を切って再接続します。

### rtsp_pull

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtsp_pull","url":"rtsp://192.0.2.10:554/stream1","credential_ref":"secret:cam-login"}'
```

- 転送方式は **TCP interleaved のみ**です（NAT やファイアウォールを越えやすい方式です）。ポートの既定は 554 です。
- 資格情報は `user:password` の形式でシークレットに保存します。Basic 認証はパスワードを平文で送るため、
  `options.allow_basic: "true"` を指定したときだけ使います。
- 社内のカメラなどプライベートアドレスへ接続するには、`--egress-allow` での許可が必要です（§6.7）。

### srt_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"srt_pull","url":"srt://contrib.example.com:9000?streamid=feed1&latency=200000","credential_ref":"secret:srt-feed-pass"}'
```

- SENCA が caller として接続します。中身は MPEG-TS です。
- `url` のクエリのうち、読み取るのは `streamid` と `latency` だけです。**`latency` の単位はマイクロ秒**です
  （ffmpeg の URL と同じで、`200000` = 200 ms）。既定は 200 ms です。
- パスフレーズは URL に書かず、`credential_ref` で渡します（10〜79 文字）。

### hls_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"hls_pull","url":"https://cdn.example.com/live/master.m3u8"}'
```

- multivariant playlist の場合は、帯域（`BANDWIDTH`）が最も大きい variant を選びます。
- TS と fMP4 のセグメントに対応しています。セグメントは実時間と同じ速さで送り直します。
- ライブの再生リストは、最初は末尾の 3 セグメントから読み込み、以後は `TARGETDURATION` の半分（0.5〜5 秒）ごとに再読み込みします。
- `#EXT-X-ENDLIST` のある再生リスト（VOD）は最後まで読み込むと終了し、状態が `ended` になります（ループ再生はしません）。
- 入力元のセグメントをバッファする分の遅延は、取り除けません。

### device

キャプチャデバイスからの入力です。[4.7 キャプチャデバイスからの取り込み](04-ingest-playback.md) を参照してください。

## 6.3 出力（push）

出力は、ストリームの配信が始まるまで `waiting_input` の状態で待機し、配信が始まると配信先へ接続します。
配信が終わると接続を閉じ、次の配信を待ちます。

### rtmp_push

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"yt-key","value":"xxxx-xxxx-xxxx-xxxx"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"rtmp_push","url":"rtmps://live.example.com:443/app","credential_ref":"secret:yt-key"}'
```

- `url` の形式は `rtmp[s]://host[:port]/{app}[/{stream key}]` です。stream key には、`credential_ref` があればその値を、
  無ければ URL の最後の部分を使います。URL に書いたキーは API の応答（`url`）にそのまま表示されるため、`credential_ref` の使用をお勧めします。
- FLV（Enhanced RTMP を含む）で送信します。ストリームのコーデックを受け付けるかどうかは、配信先によります。
- タイムアウトは、接続と publish の開始が 15 秒、各書き込みが 10 秒です。

### srt_push

```sh
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"srt_push","url":"srt://receiver.example.com:9000?streamid=senca&latency=300000","credential_ref":"secret:srt-out-pass"}'
```

- caller として接続し、MPEG-TS を 1316 バイトずつ送信します。`streamid`・`latency`（マイクロ秒）の扱いは srt_pull と同じです。
  パスフレーズは `credential_ref` で渡します。
- TS に格納できないコーデック（AV1・VP8・VP9）は送信できません。

外部の WHIP サーバーへの送信（`whip_push`）には、現在は対応していません。`protocol: "whip_push"` は 400 になります。

## 6.4 PaaS 連携（`protocol: "paas"`）

PaaS 連携では、クラウドのメディアサービスに **SENCA が取り込み用のエンドポイント（インジェストエンドポイント）を作成**し、
そこへ SENCA 自身が RTMP(S) または SRT で送信します。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cf","value":"0123456789abcdef0123456789abcdef:CF_API_TOKEN"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf","options":{"recording":"automatic"}}'
```

動作の流れ:

1. 出力が有効になると、状態が `provisioning` になり、外部のリソース（Live Input・Channel・Input）を作成します。
   リソースには `senca:owner={出力の ID}` のタグ（Cloudflare では `meta`）を付けます。作成の途中経過とリソースの ID は、
   次のリクエストを送る前に出力の `provider_resource` に保存します。このため、途中でノードが停止してもリソースが二重に作られることはありません
   （同じ owner のリソースがあれば、それを引き継ぎます）。
2. インジェストエンドポイントの秘密情報（stream key・パスフレーズ）は保存しません。送信のたびに外部の API から取得し直します。
3. `rtmp_push`・`srt_push` と同じ方法で送信します。接続方式は `options.ingest` で選べます（`rtmps` か `srt`。
   指定が無ければ連携先が返す最初のエンドポイント）。
4. **出力を削除すると、外部のリソースも削除します。** SENCA のタグが付いていないリソースは削除しません。
   後片付けに失敗しても出力の定義は削除され、失敗はログに記録されます。

外部の API への接続にも、§6.7 の宛先の制限が適用されます。

| `provider` | 作成するもの | `credential_ref` の値 | `options` |
|---|---|---|---|
| `cloudflare_stream` | Cloudflare Stream の Live Input | `account_id:api_token` | `recording`: `off`（既定）・`automatic`。`customer_subdomain`: `customer-xxxx.cloudflarestream.com`。`endpoint`: API の接続先の上書き（テスト用） |
| `aws_ivs` | Amazon IVS の Channel | `access_key_id:secret_access_key[:session_token]` | `region`（必須）。`channel_type`: `STANDARD`（既定）・`BASIC`・`ADVANCED_SD`・`ADVANCED_HD`。`latency_mode`: `LOW`（既定）・`NORMAL`。`recording_configuration_arn`。`endpoint` |
| `aws_medialive` | AWS Elemental MediaLive の Input と Input Security Group | `access_key_id:secret_access_key[:session_token]` | `region`（必須）。`input_type`: `RTMP_PUSH`（既定）・`SRT_LISTENER`。`allow_cidr`（ノードの送信元の CIDR。`,` 区切りで 10 個まで）と `input_security_group`（既存の ID。SENCA は作成も削除もしません）のどちらかが必須。`pipelines`: `1`（既定）・`2`。`rtmp_app`（既定 `senca`）。`srt_passphrase_secret_arn`。`srt_min_latency_ms`（120〜15000、既定 2000）。`endpoint` |

- Cloudflare: 送信方式は RTMPS（既定）か SRT です。
- IVS: 送信先は `rtmps://{ingestEndpoint}:443/app/` です。Channel が SRT に対応していれば SRT も選べます。
  配信中の Channel は IVS が削除を拒否します（`ConflictException`）。これで後片付けに失敗した場合はログに記録されるので、手動で削除してください。
- MediaLive: SENCA が作成するのは、送信先の Input と、それを保護する Security Group だけです。
  **Channel（エンコードの設定や出力先）は利用者が別途作成し**、この Input を接続してください。Channel に接続されたままの Input は削除できません。
  `provider_resource` の形式は `input=<id>;sg=<id>` です。

## 6.5 状態と再接続

入力・出力の `status`:

| フィールド | 意味 |
|---|---|
| `state` | `starting`・`provisioning`（PaaS）・`waiting_input`（出力が配信の開始を待っている）・`connecting`・`active`・`backoff`・`error`・`ended`（VOD の `hls_pull` を最後まで読み込んだ） |
| `last_error` | 直近の失敗の理由（秘密情報は含みません） |
| `connects` | 接続に成功した回数 |
| `since` | 現在の `state` になった時刻 |
| `bytes` | 送受信したバイト数 |

`status` に値が入るのは、その入力・出力を動かしているノードで取得したときだけです（他のノードで取得したときや、動作していないときは `null`）。

失敗すると状態は `error` になり、すぐに `backoff` に移って、指数バックオフで待機してから再接続します。
待機時間は 1 秒から倍々に増えて最大 60 秒です。実際の待機時間は、その値の半分から全部の間でランダムに決まります。
正常に終わったとき（配信元が配信を終えたときなど）は、短い待機（0.5〜1 秒）の後に次の接続を始めます。

実行例（`--egress-allow` を指定せずに 127.0.0.1 の RTSP カメラを指定した場合）:

```json
{"state": "backoff", "last_error": "127.0.0.1 は許されていない宛先です（--egress-allow で許可が必要）", "connects": 0, "bytes": 0,
 "since": "2026-11-03T01:19:43.679Z"}
```

## 6.6 シークレット（`credential_ref`）

資格情報はシークレットとして保存し、`secret:{name}` の形式で参照します。

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
```

```json
{"id":"33bb760ab08f2f11af9fcd12af258649","name":"cam-login","ref":"secret:cam-login","created_at":"2026-11-03T01:19:07.291Z"}
```

- `name` の規則はストリームの名前と同じ（1〜64 文字の英数字と `-_.`）で、テナント内で一意です（重複すると 409）。`value` は 1〜8192 バイトです。
- 値は AES-256-GCM で暗号化して保存します。暗号化の鍵は、単一ノードではノードの鍵 `{data-dir}/control/secret-key`、
  クラスターではクラスター共通の鍵です（[12. セキュリティ](12-security.md)）。**保存した値は API から読み出せません**
  （一覧に表示されるのは `id`・`name`・`ref`・`created_at` だけです）。値を変えるには、削除して作り直してください。
- 1 つのテナントに保存できるシークレットは 500 個までです。
- 入力・出力・ストリームの字幕から参照されているシークレットは削除できません（409）。
- 存在しないシークレットを参照すると、入力・出力の作成・変更は 400 になります。

| 使用箇所 | 値の形式 |
|---|---|
| `rtmp_pull` | stream name |
| `rtmp_push` | stream key |
| `rtsp_pull` | `user:password` |
| `srt_pull`・`srt_push` | パスフレーズ（10〜79 文字） |
| `paas` | §6.4 の表を参照 |
| 字幕（[8. 字幕と翻訳](08-captions.md)） | API キー |
| 名前が `srt-passphrase` のもの | SRT の待ち受け（listener）のパスフレーズ（[4.5](04-ingest-playback.md)） |

## 6.7 外向きの接続の制限（egress）

pull の入力、push の出力、PaaS の API、字幕の API の接続先は、名前解決を**した後の IP アドレス**で検査します
（DNS でプライベートアドレスを指すように設定しても、この検査は回避できません）。既定で拒否する宛先は次のとおりです。

- ループバック（127.0.0.0/8、::1）、未指定アドレス、マルチキャスト、ブロードキャスト、0.0.0.0/8
- リンクローカル（169.254.0.0/16。クラウドのメタデータ 169.254.169.254 を含みます。fe80::/10）
- プライベートアドレス（10/8、172.16/12、192.168/16、100.64/10、fc00::/7）。IPv4 射影 IPv6 アドレスも同様です

社内のカメラや配信先へ接続するときは、管理者が `--egress-allow CIDR` で範囲を許可します（複数回指定できます）。

```sh
senca-node ... --egress-allow 192.0.2.0/24 --egress-allow 10.20.0.0/16
```

許可されていない宛先は、入力・出力の作成時ではなく**接続時**に失敗し、`last_error` に理由が表示されます（§6.5 の例）。

## 6.8 配置先（グリッド）

`enabled: true` の入力・出力は、グリッドのワークロード（`input:{id}`・`output:{id}`）になります。
スケジューラーが割り当てたノードだけが、そのワークロードを動かします。

- 出力は、そのストリームの配信を受信しているノードに優先して配置されます（ノード間の中継を 1 段省けるためです）。
- キャプチャの入力は、そのデバイスを持つノードに配置されます。H.264/H.265 でエンコードするキャプチャは、GPU のエンコードセッションを 1 つ予約します。
- `placement` では、手動の割り当て（`node`・`device`）、ラベル、アンチアフィニティ、優先度、配置の方針（`spread`・`binpack`）、
  割り当て先が停止したときの扱い（`pin_policy`）を指定できます。`degrade` はパイプライン専用で、入力・出力に指定すると 400 になります。
- 配置の結果は `placement_status` に表示されます（`state`: `running`・`pending`・`unschedulable`・`not_scheduled`。ほかに `node`・`node_name`・`reason` など）。

詳しくは [9. クラスターとグリッド](09-cluster-grid.md) を参照してください。単一ノードでも同じ仕組みで動作します（配置先はそのノードだけです）。
