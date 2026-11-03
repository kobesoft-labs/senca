# 9. クラスタとグリッド

[English](../en/09-cluster-grid.md)

複数の senca-node を 1 つのクラスタにして、制御状態を複製し、入力・出力・パイプライン・部屋を
グリッドで各ノードへ配置する。ノードや装置が止まっても、仕事は生きているノードへ移るか縮退して動き続ける。

- 設計の理由: [ADR-0009 分散の制御面](../../adr/0009-cluster.md)、[ADR-0013 グリッド](../../adr/0013-grid.md)、[ADR-0014 中継](../../adr/0014-relay.md)
- 関係するページ: [3. 設定](03-configuration.md)、[6. 入力と出力](06-inputs-outputs.md)、[7. パイプライン](07-pipelines.md)、[10. 部屋](10-rooms.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)、[15. API と SDK](15-api-and-sdks.md)

単一ノードでもグリッドは同じ仕組みで動く（自分がリーダーで、配置先は自分だけ）。配置の指定（`placement`）・
縮退・`GET /api/v1/grid` は単一ノードでも使える。

## 9.1 全体の形

| 部品 | 何をするか |
|---|---|
| Raft（`senca-raft`） | 制御状態のすべての変更（コマンド）を複製する。投票者は 1〜7 台 |
| 所有権 | 配信を受けたノードが stream の所有者になる（fencing token つき） |
| 中継（ADR-0014） | 所有者でないノードの視聴者には、所有者から AU を運んで手元の「写し」から配る |
| グリッド（`senca-grid`） | リーダーが心拍で停止を見つけ、毎秒、仕事の配置を決めて合意ログに載せる。各ノードは自分の分だけを動かす |

複製されるのは**制御状態だけ**。メディア（HLS のセグメント、録画のファイル）はそれを作ったノードのディスクにあり、
複製しない（[5. 録画](05-recording.md)）。

## 9.2 クラスタを組む

### 設定ファイル

クラスタの設定は JSON で、`--cluster-config FILE` で渡す（または `--config` の JSON の `cluster` に同じものを書く）。
未知のフィールドは起動時のエラーになる。

```json
{
  "node_id": 1,
  "listen": "10.0.0.1:7000",
  "relay_listen": "10.0.0.1:7001",
  "secret_file": "/etc/senca/cluster-secret",
  "tls_ca": "/etc/senca/cluster-ca.pem",
  "tls_cert": "/etc/senca/n1.pem",
  "tls_key": "/etc/senca/n1.key",
  "peers": [
    {"id": 1, "addr": "10.0.0.1:7000", "relay": "10.0.0.1:7001", "api": "https://n1.example.com"},
    {"id": 2, "addr": "10.0.0.2:7000", "relay": "10.0.0.2:7001", "api": "https://n2.example.com"},
    {"id": 3, "addr": "10.0.0.3:7000", "relay": "10.0.0.3:7001", "api": "https://n3.example.com"}
  ]
}
```

| フィールド | 必須 | 意味 |
|---|---|---|
| `node_id` | はい | このノードの Raft の番号。1 以上で、`peers` の中に同じ `id` が要る |
| `listen` | はい | ノード間の通信（Raft・転送・心拍）の待ち受け |
| `peers` | はい | 投票するノード（自分を含む）。1〜7 台 |
| `peers[].id` | はい | Raft の番号 |
| `peers[].addr` | はい | そのノードの `listen` |
| `peers[].api` | いいえ | そのノードの管理 API の URL。307 の案内・部屋の `redirect`・部屋の REST の転送に使う（下記） |
| `peers[].relay` | いいえ | そのノードの中継の待ち受け。既定は `addr` のポート + 1（下の検査を参照） |
| `secret_file` | はい | クラスタの合言葉のファイル。全ノードで同じ内容。前後の空白を除いて 16 文字以上 |
| `tls_ca`・`tls_cert`・`tls_key` | いいえ | ノード間の mTLS。3 つそろえるか、3 つとも書かない（一部だけはエラー） |
| `relay_listen` | いいえ | このノードの中継の待ち受け。既定は `listen` のポート + 1（下の検査を参照） |

次の場合は起動しません（終了コード 2 または 1）:

- `node_id` が 0、または `peers` に自分が含まれていない。`peers[].id` が 0 か重複している。
- `peers` が 0 台か 8 台以上。
- 合言葉が 16 文字未満。
- `tls_*` が一部だけ。
- **アドレスが重なっている**: いずれかのノードの中継アドレス（`peers[].relay`、省略時は `addr` + 1）が、別のノードの
  Raft のアドレス（`peers[].addr`）または中継アドレスと同じ。同じホストで Raft のポートを連番（7000・7001・7002）にすると、
  既定の中継ポート（+ 1）が隣のノードの Raft と重なります。その場合は `relay_listen` と `peers[].relay` を明示してください。
- `listen` または `relay_listen` で待ち受けできない（ポートが使用中など）。

注意:

- **すべてのノードが投票者になる。** 投票しないメディア専用のノードは今は作れないので、クラスタは最大 7 台。
- **投票者の顔ぶれは設定ファイルで固定。** 動かしながら足す・外す API は無い。新しいクラスタは、すべての投票者が
  同じ `peers` で初めて起動したときに作られる（データ置き場の `control/raft/` が空のとき）。
- `peers[].api` は、**利用者の端末と他のノードの両方から届く URL** にする。ノードの登録（最初の起動）のときに記録され、
  後で変えても登録済みの値は変わらない。書かなければ 307 の案内と部屋の別ノードへの案内ができない。
- 単一ノードで使っていたデータ置き場をそのままクラスタにしても、制御状態は引き継がれない（分散では Raft のログだけを使い、
  単一ノードのログ `control/log` は読まない）。

### ポート

| ポート | 相手 | 用途 |
|---|---|---|
| `listen` | 他のノードだけ | Raft・提案の転送・グリッドの心拍 |
| `relay_listen`（既定 `listen` + 1） | 他のノードだけ | 中継（AU の運搬） |
| `--http`・`--https` | 利用者と他のノード | 管理 API・HLS・シグナリング。`peers[].api` の宛先 |
| `--rtmp`・`--srt`・`--rtsp`・`--webrtc-udp` など | 利用者 | 配信・視聴（[4. 受信と再生](04-ingest-playback.md)） |

### 合言葉と mTLS

ノード間の接続（Raft と中継）は、接続の直後にクラスタシークレットを使った**相互のチャレンジレスポンス認証**を行います。
双方が新しい乱数を送り、シークレットから導出した鍵による HMAC（双方の乱数とノード番号を含む）で、シークレットを知って
いることを証明します。シークレット自体も固定の値も送らないため、通信を盗聴して再送しても接続できません。相手のノード番号が
接続先と違う場合、MAC が一致しない場合、5 秒以内に終わらない場合は接続を閉じます（ログに
`cluster: rejected connection from ...`・`relay: connection from ... refused` と出ます）。

`tls_*` を設定すると、その下で mTLS を使います。クラスタの認証局（`tls_ca`）が発行した証明書を双方が検証し、証明書の SAN
には **`senca-cluster`**（DNS 名）が必要です（チャレンジレスポンス認証は TLS の中でも行います）。`tls_*` を設定しない場合は
平文の TCP になり、複製される制御状態と中継のメディアは暗号化されません（[12. セキュリティ](12-security.md)）。
中継の接続も同じ証明書を使います。

**バージョンの混在**: チャレンジレスポンス認証はこのバージョンで導入されました。以前のバージョンのノード（固定のハッシュを
送る方式）とは接続できないため、クラスタのノードはまとめて更新してください。

`tools/interop/cluster.sh` と同じ手順の例（P-256、各ノードの証明書は serverAuth と clientAuth の両方）:

```sh
# クラスタの認証局
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout ca.key -out ca.pem -days 365 -subj "/CN=senca-cluster-ca"
# ノードごと（n1・n2・n3）
for n in n1 n2 n3; do
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout $n.key -out $n.csr -subj "/CN=$n"
  printf "subjectAltName=DNS:senca-cluster\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\n" > $n.ext
  openssl x509 -req -in $n.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out $n.pem -days 365 -extfile $n.ext
done
# 合言葉（全ノードで同じファイル）
openssl rand -hex 32 > cluster-secret
```

別の認証局の証明書を持つノードは、合言葉を知っていても接続を拒まれる（ログに `mTLS from ... refused`）。

### 起動

```sh
senca-node --data-dir /var/lib/senca --node-name n1 \
  --http 0.0.0.0:8080 --public-http-url https://n1.example.com \
  --cluster-config /etc/senca/cluster-n1.json \
  --label zone=a --net-mbps 10000
```

- `--node-name` はノードの名前。配置の指定や API でノードを名前で指せる。ノードの ID（`data_dir/node-id` に保存される
  128 ビットの ID）とは別。
- 起動すると、リーダーが決まってから各ノードが自分を登録する。ID が最小のノード（例では `id: 1`）だけが、再生 URL の
  鍵と最初の管理者トークンを作る。トークンはそのノードの `data_dir/control/bootstrap-admin-token`（mode 0600）に書かれ、
  どのノードでも使える（複製される）。
- 初期化が済むまで `/readyz` は 200 になりません。その後も、過半数（またはリーダー）との通信が 4 秒を超えて途絶えている
  間は 503 を返します（[13. 運用](13-operations.md)）。
- 秘密値を暗号化するクラスタ共通の鍵がまだ無い場合、各ノードが自分の鍵を暗号化して提案し、最初に適用されたものが
  使われます（[12. セキュリティ](12-security.md)）。

### 書き込みと読み込み

- **どのノードに API を送ってもよい。** 書き込みはリーダーへ転送され、合意の後に全ノードで同じ順に適用される。
- 適用の前に手元の状態で試すので、明らかな誤り（400・404・409 など）はすぐに返る。確定の結果は合意後の適用で決まる。
- リーダーが決まっていない・過半数と連絡が取れない・5 秒以内に適用されないときは
  **503 `CAPACITY_UNAVAILABLE`（`retryable: true`）**。過半数を失った側は書き込めない。
- **読み込みはそのノードの手元の状態から**返す（リーダーに問い合わせない）。書いた直後の数 ms は古いことがある。
  過半数を失ったノードでも読める。
- Raft の時間: tick 100 ms、選挙のタイムアウトは 1〜2 秒（10〜20 tick の乱数）、1000 件ごとにスナップショット。
  ログは `data_dir/control/raft/` に fsync してから送信・適用する。書けなければノードは終了する（終了コード 70）。

### 複製されるもの

`ControlState` のすべて: stream、API トークン（ハッシュ）、録画の記録（メタデータ）、予約、Idempotency-Key の記録、
ノードの登録（`GET /nodes`）、入力・出力、秘密（暗号化した値）と秘密値の鍵（クラスタシークレットから導出した鍵で暗号化したもの）、
stream の所有者、再生 URL の鍵、グリッドの状態
（ノードの申告・仕事・割り当て・置けない理由）、部屋、メディアパイプライン。

複製されないもの: メディアのデータ（ライブのセグメント・録画のファイル）、グリッドの心拍、部屋の参加者と出来事
（部屋を置いたノードの手元）。

## 9.3 stream の所有権と、どのノードでも見られる仕組み

- 送り手（RTMP・SRT・WHIP など、または pull の入力）がつながったノードは `ClaimStream` を提案する。適用で新しい
  fencing token が払い出され、そのノードが所有者になる（SSE の `stream.owner`）。
- 他のノードはこの適用を見て、同じ stream の送り手を手元で受けていれば切る（ログに `publisher evicted`）。
  同じ stream が 2 つのノードで同時に公開されることはない。後からつないだ方が勝つ。
- 送り手はどのノードにつないでもよい。配信を受け直すと所有者はそのノードに移る。
- **入力が終わると所有権を解放します。** 送り手が切断され、再接続の猶予（`--reconnect-grace`）が過ぎると、所有者のノードは
  `ReleaseStream` を提案します（token が現在のものと同じ場合だけ解放されます。ログに `released ownership of stream ...`）。
  以降、他のノードの視聴者は停止した所有者へリダイレクトされず、404 になります。
- **停止と判定したノードの所有権も解放します。** `NodeDown` が適用されると、そのノードが持っていた stream の所有者の記録を
  削除します。ノードが復帰して送り手を受け続けていれば、そのノードが所有権を取り直します（ログに `re-claimed stream ...`）。

### 視聴: 中継の写し

所有者でないノードに視聴が来たら（署名付きの HLS、RTMP・SRT・RTSP の play、WHEP、パイプラインの入力）、そのノードは
所有者から中継で AU を受け、手元に stream の**写し（mirror）**を作って配る。

- 同じ stream を何人が見ても、ノード間で運ぶのは 1 本。写しのパッケージャー・GOP キャッシュは所有者と同じ動き。
- 運ぶのは符号化済みの AU だけ（変換しない）。
- 写しは録画・字幕・所有権の主張をしない（所有者のノードの仕事）。
- 作ったばかりの写しは、最初の再生リストができるまで最大 6 秒待つ。
- 使われなくなって 30 秒たった写しは片付ける。所有権がこのノードへ移ったら、写しを止めて本物の送り手に譲る。
- 所有者が別のノードへ移ると、写しは新しい所有者から受け直す。その間の HLS の再生リストには不連続が入る。

### 307 の案内（予備の経路）

- ライブ: 写しが作れない・6 秒以内に再生リストが出ないとき、手元に再生リストが無ければ、所有者のノードの
  `peers[].api` の同じパス（`/s/...`）へ **307** で案内する。
- 録画（VOD）: 録画のファイルは録ったノードにある。手元に無ければ、録ったノードへ 307 で案内する。
- 案内先の URL は登録時の `peers[].api`。書いていなければ案内できず、404 などになる。

中継の状態は `GET /api/v1/nodes` の自分のノード（`"self": true`）の `relay` に出る:
`peers[]`（`node`・`rtt_ms`・`tx_bps`・`rx_bps`・`tx_bytes`・`rx_bytes`・`tx_aus`・`rx_aus`・`overhead_per_au`・
`upstreams`・`downstreams`・`going_away`）と `mirrors`（写しの stream ID の並び）。

## 9.4 グリッド

### ノードの登録と容量

各ノードは起動時（とその後 30 心拍ごとに見直して、変わったとき）に、仕事に貸せる資源をグリッドへ申告する。

| 資源 | 既定 | 変える |
|---|---|---|
| CPU（ミリコア） | 論理 CPU 数 × 1000 × 90% | `--capacity-cpu-millis N` |
| メモリ（MB） | `/proc/meminfo` の MemTotal の 85%（読めなければ 4096 MB を基に） | `--capacity-memory-mb N` |
| 帯域（Mbit/s、上り・下りそれぞれ） | 10000 | `--net-mbps N` |
| GPU | コーデックの worker が見つけたハードウェアの符号化・復号。装置ごとに 1 つ（ID は `nvidia:0`・`intel-vpl:0`・`amd-amf:0`・`vaapi:0` の形）。セッション数は分かればその値、分からなければ符号化 8・復号 16 | — |
| キャプチャ機器 | キャプチャの worker が見つけた機器 | — |
| ラベル | `--label KEY=VALUE`（何度でも）。`senca.io/name` には `--node-name` が自動で入る | `--label` |

- `--label` のキー・値は 1〜128 文字。`senca.io/` で始まるキーは予約（コマンドラインでは拒否）。
- 設定ファイル（`--config`）では `net_mbps`・`labels`・`capacity_cpu_millis`・`capacity_memory_mb` に同じものを書ける。
  設定ファイルの `labels` も `--label` と同じ規則で検査します（キーは 1〜128 文字、値は 128 文字まで、制御文字は使用不可、
  `senca.io/` で始まるキーと `raft_id` は予約、64 個まで）。規則に合わない場合は起動しません。
- ノードの役割は `media`（仕事を動かす）と `control`（投票者）。今はすべてのノードが両方を持つ。
- **配置は予約量で計算します。** 予約量はワークロードごとの見積もり（9.5）の合計で、実際の使用量ではありません。
  ハートビートには実測の使用量が含まれます（CPU は `/proc/stat` の差分からミリコア、メモリは `/proc/meminfo` の
  MemTotal − MemAvailable、帯域はループバックを除く `/proc/net/dev` の差分から Mbit/s）。スケジューラー（リーダー）の
  `GET /api/v1/grid` の `usage` で確認できます（表示用で、配置には使いません）。

### 心拍と停止の検出

- 各ノードは **1 秒ごと**に心拍をリーダーへ送る（クラスタの接続で。合意ログには載せない）。心拍には、手元で受けている
  stream・動かしている仕事・装置の健康が入る。リーダーは、送り手のノードの Raft の番号と登録の `raft_id` が合う心拍だけを受ける。
- リーダーは 1 秒ごとに、最後の心拍から **5 秒を超えた**ノードを停止とみなして `NodeDown` を、戻れば `NodeUp` を
  合意ログに載せる（SSE の `node.down`・`node.up`）。
- リーダーが替わったら検出器を作り直す。作ってから 5 秒は、心拍をまだ受けていないノードも生きているとみなす。
- デバイスの健全性: 各ノードは 10 秒ごとにデバイスを再検出します（新しい codec worker を 1 つ起動して Hello の能力を読み、
  capture worker にデバイス一覧を問い合わせます）。登録済みのデバイスが検出されなくなるとハートビートで異常を伝え、リーダーが
  `DeviceDown`（SSE の `device.down`）を、再び検出されると `DeviceUp` をログに記録します。異常なデバイス上のワークロードは
  別のデバイスへ移動するか縮退します。検出されなくなったデバイスも登録からは削除しません（異常のまま残ります）。
  新しく検出されたデバイスは再登録（30 ハートビートごと）で追加されます。手動の無効化（9.7 の `disable`）とは別のもので、
  健全性の測定結果では解除されません。
- 停止の知らせを適用した次の周期に配置を計画する。仕事が増えた・消えた・変わったときは周期を待たずに計画する。

### 仕事

グリッドで配置する仕事は、資源から自動で作られる（利用者が直接は作らない）。仕事の ID は資源の種類と ID で、
`input:<id>`・`output:<id>`・`pipeline:<id>`・`room:<id>`。

| 資源 | 仕事になる条件 | 種類 |
|---|---|---|
| 入力（pull） | `enabled: true` | `ingest`（`protocol: "device"` は `capture`） |
| 出力（push） | `enabled: true` | `egress` |
| パイプライン | `desired_state: "running"` | `pipeline` |
| 部屋 | いつも | `room` |

仕事にならないもの: 送り手がつなぐ push の配信（つないだノードで受ける）、録画（所有者のノードで録る）。
部屋のメディアパイプラインの書き出しは部屋の見積もりに足され、合成は部屋と同じノードに固定したパイプラインになる
（[11. メディアパイプライン](11-media-pipelines.md)）。

各ノードは割り当てのうち自分の分だけを動かす。割り当てが別のノードへ移ると token が変わり、古いノードは
その適用を見た時点で止める。

### 配置の指定（`placement`）

入力・出力・パイプライン・部屋の作成と PATCH で `placement` を書ける。結果は各資源の `placement_status` に出る。

```json
{
  "node": "n2",
  "device": "nvidia:0",
  "labels": [
    {"op": "eq", "key": "zone", "value": "a"},
    {"op": "in", "key": "rack", "values": ["r1", "r2"]},
    {"op": "not_in", "key": "tier", "values": ["spot"]}
  ],
  "anti_affinity": "camera-1",
  "pin_policy": "prefer",
  "priority": "high",
  "strategy": "spread",
  "degrade": [
    {"label": "no_av1", "disable": ["e3"]},
    {"label": "single", "disable": ["sc"]}
  ]
}
```

| フィールド | 既定 | 意味 |
|---|---|---|
| `node` | 無し | 手動の割り当て。ノードの ID か名前（`--node-name`）。保存するときに ID に直す。無いノードは 400 |
| `device` | 無し | 手動の割り当て（GPU の ID）。`node` が無ければ、その GPU を持つどれかのノード |
| `labels` | 無し | ラベルの条件（すべて満たすノードだけ）。最大 16 個 |
| `anti_affinity` | 無し | 組の名前。同じ組の仕事は同じノードに置かない |
| `pin_policy` | `prefer` | 手動の割り当て先が止まったとき。`prefer` は他へ移し、戻って冷却が過ぎたら戻す。`strict` は止めたまま |
| `priority` | `normal` | `low` < `normal` < `high` < `critical` |
| `strategy` | `spread` | `spread` は置いた後の空きが多いノードへ、`binpack` は詰める |
| `degrade` | 無し | 縮退の段（**パイプラインだけ**。他の資源に書くと 400）。最大 8 段 |

ラベルの条件:

| `op` | 形 | 合うノード |
|---|---|---|
| `eq` | `{"op":"eq","key":K,"value":V}` | ラベル K が V |
| `in` | `{"op":"in","key":K,"values":[...]}` | ラベル K が並びのどれか（K が無いノードは合わない） |
| `not_in` | `{"op":"not_in","key":K,"values":[...]}` | ラベル K が並びのどれでもない（K が無いノードも合う） |

検査（違反は 400 `INVALID_CONFIG`）: `node`・`device`・`anti_affinity`・ラベルのキーと値は 1〜128 文字で制御文字を含まない。
`in`・`not_in` の `values` は 1〜32 個。未知のフィールドは 400。

`degrade` の段:

- `label` は 1〜128 文字。`full`（段 0 の名前）は使えず、重複もできない。
- `disable` は空にできず、図にある節点の ID だけ。
- 段は**積み重なる**（段 2 は段 1 で止めた節点も止める）。止めた節点の下流は、入力がすべて止まれば止まる。
  入力の一部だけが止まる節点（複数入力の sink・合成・音の混ぜ合わせ）は、止まった入力を外して残る。

手動の割り当て（pin）の動き:

- 割り当て先（ノード・GPU）が生きている間は、`strict` も `prefer` もそこにしか置かない（排出中なら新しくは置かない）。
- 割り当て先が止まると、`strict` は置けないまま（理由 `pin_target_down`）、`prefer` は他のノードへ移る。
- `prefer` で他へ移った仕事は、割り当て先が戻り、移ってから 30 秒（冷却）が過ぎたら戻る（理由 `pin_returned`）。

### 点数

空きに置くとき、候補のノードの点数が高い方を選ぶ（同点は ID の小さいノード）。

- `spread`: 置いた後の空きの割合（千分率。CPU・メモリ・帯域・GPU のセッションの最も小さいもの）。`binpack` は 1000 から引いたもの。
- データのある所: 仕事の入力の stream（出力はその stream、パイプラインは `source` の stream）を受けているノードに +10000。

## 9.5 仕事ごとの見積もり

予約は `crates/senca-control/src/workloads.rs` の固定の見積もり。CPU はミリコア、帯域は Mbit/s。

| 仕事 | CPU | メモリ | 下り（受信） | 上り（送信） | その他 |
|---|---|---|---|---|---|
| 入力（pull） | 150 | 64 MB | 10 | 0 | — |
| 入力（`protocol: "device"`） | 500 | 64 MB | 0 | 0 | キャプチャ機器を専有。`options.codec` が H.264/H.265 なら GPU の符号化 1 |
| 出力 | 100 | 64 MB | 0 | 10 | 入力の stream を受けているノードを好む |
| 部屋（参加者の上限 N） | 200 + 20N | 64 + 2N MB | 2N | 2N × min(N, 8) | 動いているメディアパイプラインごとに CPU + 20N、メモリ + 8N MB |
| パイプライン | 下の表の和 | 下の表の和 | 10 | 10 | GPU のセッションはすべて同じ装置に置く |

部屋の N は `settings.max_participants`（無ければ 50、1〜10000 に丸める）。

パイプラインの見積もり（基本 CPU 200・メモリ 128 MB に、節点ごとに足す）:

| 節点 | CPU | メモリ | GPU |
|---|---|---|---|
| `encode`（H.264・H.265） | +100 | +128 MB | 符号化 +1 |
| `encode`（Opus） | +50 | +128 MB | — |
| `encode`（AV1・VP8・VP9 などソフトウェアの映像） | +1500 | +128 MB | — |
| `decode`（入力の `source` のコーデックが H.264・H.265） | +100 | +128 MB | 復号 +1 |
| `decode`（その他） | +400 | +128 MB | — |
| `compose`・`board`・`scale`・`crop` | +300 | +64 MB | — |
| `audio_mix` | +50 | — | — |

例: VP9 を復号し、AV1 を 2 本符号化し、1 本は `scale` を通す図は 200 + 400 + 1500 × 2 + 300 = 3900 ミリコア。
`scale` を止める段（その下流の符号化も止まる）は 2100 ミリコア（`tools/interop/grid_single.sh`）。

H.264・H.265 はソフトウェアの実装を持たない（[14. コーデックと特許](14-codecs-and-patents.md)）ので、それを使う
パイプラインは GPU の無いノードには置けない（理由 `no_device`）。

## 9.6 縮退・追い出し・戻し

配置係はリーダーで毎秒動き、同じ入力からはいつも同じ決定を出す（`crates/senca-grid/src/plan.rs`）。

1. **今の割り当ての検査**: ノードの停止・装置の故障・条件の変更・反親和性で合わなくなったものを置き直しに回す。合うものは動かさない。
2. **容量の超過の修理**: ノードの容量が減って予約が超えたら、優先度の低い順に段を下げ、それでも足りなければ追い出す（理由 `overcommitted`）。
3. **優先度の高い順**（critical → high → normal → low）に、置き直しと新しい仕事を置く。各段（full から）で次の順に試す。
   1. 空きに置く。
   2. **低い**優先度の仕事の段を下げる。
   3. **低い**優先度の仕事を追い出す。
   4. どれも駄目なら自分の次の段（縮退）で同じことを試す。
   縮退・追い出しの案は、追い出す数 → 追い出す仕事の優先度の和 → 下げる段の数、が小さいものを選ぶ。
   **同じか高い優先度の仕事は押しのけない**（置けなければ `unschedulable`、理由 `contended`）。
4. 縮退している仕事は、段が変わってから **30 秒（冷却）**が過ぎたら元の段へ戻す（理由 `capacity_returned`）。
   同じノードの空き → 他のノードの空きへ移す → 同じノードで低い優先度の仕事を縮退・追い出し、の順。
5. **任意の移動**: 排出中のノードの仕事と、`prefer` の割り当て先が戻った仕事を移す（1 回の計画で 64 件まで）。

`PlanConfig` の既定: `restore_cooldown_ms` 30000、`restore_headroom_permille` 0（戻した後の余裕を求めない）、
`max_voluntary_moves` 64、`locality_bonus` 10000。設定では変えられない。

段を下げる・追い出すのは、高い優先度の仕事のため、またはノード・装置の故障・容量の減少のときだけ。段を良くするのは
冷却が過ぎた後だけ（行ったり来たりを抑える）。

縮退したパイプラインは、その段で止める節点を図から外して起動し直される（ログに
`pipeline <id>: running degraded (level N, without [...])`）。縮退の段を持たない仕事（入力・出力・部屋）は、
足りなければ置けないか追い出されるだけ。

### `placement_status`

```json
{"state": "degraded", "node": "<node-id>", "node_name": "n2", "device": null,
 "level": 1, "level_label": "single", "since": "2026-11-03T10:00:00Z",
 "reason": {"code": "preempted", "by": "pipeline:..."}}
```

| `state` | 意味 | 他のフィールド |
|---|---|---|
| `running` | full で動いている | `node`・`node_name`・`device`・`level`・`level_label`・`since`・`reason` |
| `degraded` | 縮退した段で動いている | 同上 |
| `pending` | 配置を待っている | `reasons`（null） |
| `unschedulable` | どのノードにも置けない | `reasons` |
| `not_scheduled` | 仕事が無い（無効の入力・出力、止めたパイプライン） | — |

`reason.code`: `placed`・`no_capacity`・`node_down`（`node`）・`device_down`（`device`）・`draining`・
`constraint_changed`・`preempted`（`by`）・`overcommitted`・`capacity_returned`・`pin_returned`・`pin_target_down`。

`reasons` は置けない理由ごとのノードの数と例（最大 8 件）: `[{"reason": "cpu", "nodes": 2, "examples": ["<node-id>", ...]}]`。
理由: `node_down`・`not_media`・`draining`・`pinned_elsewhere`・`pin_target_down`・`labels`・`anti_affinity`・
`no_device`・`capture`・`cpu`・`memory`・`net_in`・`net_out`・`gpu_encode`・`gpu_decode`・`gpu_memory`・
`contended`（単独なら収まるが、同じか高い優先度の仕事で埋まっている）。

## 9.7 運用の操作

すべて `node:admin` の権限が要る（`tenant:admin` には含まれない）。

| 操作 | API | 応答 |
|---|---|---|
| ノードの一覧・1 件 | `GET /api/v1/nodes`・`GET /api/v1/nodes/{id}` | 200 |
| 排出 | `POST /api/v1/nodes/{id}/drain` | 202 |
| 排出の解除 | `POST /api/v1/nodes/{id}/undrain` | 202 |
| 装置を手で止める | `POST /api/v1/nodes/{node}/devices/{device}/disable` | 202 |
| 装置を戻す | `POST /api/v1/nodes/{node}/devices/{device}/enable` | 202 |
| グリッドの様子 | `GET /api/v1/grid` | 200 |

- **排出**（drain）: そのノードに新しい仕事を置かず、置いてある仕事を空きのある他のノードへ移す（理由 `draining`。
  1 回の計画で 64 件まで）。そのノードに pin した仕事は移さない。`{id}` はノードの ID（名前は使えない）。
  解除しても、移した仕事は自動では戻らない（`prefer` の pin を除く）。
  drain 中のノードは**新しい送り手を拒否します**: RTMP の publish は `NetStream.Publish.Rejected`（説明 `this node is draining`）、
  SRT の publish はハンドシェイクで拒否（理由 1503 = unavailable）、WHIP の `POST /whip/{stream}` は drain 中でない別の
  ノードの同じパスへ **307** でリダイレクトします（該当するノードが無ければ 503 と `Retry-After`）。接続済みの送り手と
  視聴者はそのまま継続します。
- **装置の停止**: GPU かキャプチャ機器を、測った健康とは別に止める。止めた装置の仕事は別の装置へ移るか縮退する。
  心拍でもノードの申告し直しでも戻らない（手で戻すまで）。`{node}` は ID か名前。知らない装置は 404。
  応答は `{"node": "<id>", "device": "nvidia:0", "enabled": false}`。
- ノードをグリッドから消す API は無い。止めたままのノードは `up: false` で残る。

### `GET /api/v1/grid`

```json
{
  "scheduler": "<node-id> または null",
  "nodes": [{
    "id": "<node-id>", "name": "n1", "up": true, "draining": false,
    "roles": ["control", "media"], "labels": {"senca.io/name": "n1", "zone": "a"},
    "capacity": {"cpu_millis": 7200, "memory_mb": 13000, "net_in_mbps": 10000, "net_out_mbps": 10000},
    "reserved": {"cpu_millis": 350, "memory_mb": 192, "net_in_mbps": 20, "net_out_mbps": 10},
    "gpus": [{"id": "nvidia:0", "vendor": "nvidia", "healthy": true, "enabled": true,
              "encode_sessions": 8, "decode_sessions": 16, "encode_reserved": 1, "decode_reserved": 0}],
    "capture": [{"id": "...", "kind": "...", "healthy": true}],
    "usage": {"cpu_millis": 850, "memory_mb": 2300, "net_in_mbps": 12, "net_out_mbps": 40},
    "last_heartbeat": "2026-11-03T10:00:00Z",
    "workloads": 3
  }],
  "workloads": [{
    "id": "input:...", "kind": "ingest", "priority": "high", "pinned": true,
    "state": "running", "node": "<node-id>", "device": null, "level": 0, "level_label": "full",
    "reason": {"code": "placed"}, "unschedulable": null
  }]
}
```

- `scheduler` は、問い合わせたノードがリーダー（配置係）なら自分の ID、そうでなければ null。
- `last_heartbeat` と `usage`（最後のハートビートの実測の使用量）はリーダーだけが保持しています。リーダー以外に問い合わせると null です。
- `reserved` は予約（割り当ての見積もりの和）。実際の使用量ではない。
- `workloads[].state` は `running`・`degraded`・`unschedulable`・`pending`。
- `capture[]` の `disabled` は手で止めたときだけ出る。

### SSE のイベント

`GET /api/v1/events`（[15. API と SDK](15-api-and-sdks.md)）に流れる。グリッドとノードのイベントはテナントの外なので、
`node:admin` を持つトークンにだけ見える。

| イベント | `resource_id` | `data` |
|---|---|---|
| `node.updated` | ノードの ID | `{"draining": bool}`（登録・排出） |
| `node.down`・`node.up` | ノードの ID | `{}` |
| `device.down`・`device.up` | ノードの ID | `{"device": ...}`（測った健康の変化） |
| `device.disabled`・`device.enabled` | ノードの ID | `{"device": ...}`（手の操作） |
| `grid.decisions` | 空 | `{"decisions": [...]}` |
| `stream.owner` | stream の ID | `{"node": "<node-id>"}` |

`grid.decisions` の各要素は `type` で区別する: `assign`（`workload`・`node`・`device`・`level`・`token`・`reason`）、
`move`（さらに `from`）、`degrade`・`restore`（`workload`・`level`・`device`・`reason`）、`evict`（`workload`・`reason`）、
`unschedulable`（`workload`・`reasons`）。

## 9.8 例

pull の入力を n3 に置き、止まったら他へ移す:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  https://n1.example.com/api/v1/streams/$SID/inputs \
  -d '{"protocol":"rtmp_pull","url":"rtmp://src.example.com/live/key",
       "placement":{"node":"n3","pin_policy":"prefer","priority":"high"}}'
curl -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/inputs/$IID | jq .placement_status
```

zone が a か b のノードに、2 本のカメラの入力を別々のノードへ:

```json
{"protocol":"srt_pull","url":"srt://cam1.example.com:9000",
 "placement":{"labels":[{"op":"in","key":"zone","values":["a","b"]}],"anti_affinity":"cameras"}}
```

足りなければ縮退するパイプライン（`placement` の部分）:

```json
{"priority":"normal","degrade":[{"label":"single","disable":["sc"]}]}
```

ノードを保守に入れる:

```sh
NID=$(curl -s -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/grid \
  | jq -r '.nodes[] | select(.name=="n2") | .id')
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/drain
# 保守の後
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/undrain
```

GPU を手で止める・戻す:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/disable
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/enable
```

## 9.9 故障の場合と、かかる時間

時間は上の定数（心拍 1 秒、停止の判定 5 秒、選挙 1〜2 秒、冷却 30 秒）から決まる。

| 場合 | 何が起きるか | 時間の目安 |
|---|---|---|
| リーダーでないノードが止まる | 心拍が途絶えて 5 秒を超えると `node.down`。次の周期で仕事を移す（`move`、理由 `node_down`）。移った先のノードが仕事を始める。`strict` の pin は移さない | 停止から移動まで約 6〜9 秒（`tools/interop/cluster.sh` での実測） |
| リーダーが止まる | 選挙の間は書き込みが 503（retryable）。新しいリーダーは検出器を作り直し、5 秒の猶予の後に止まったノードを `node.down` にする | 選挙 1〜2 秒 + 猶予 5 秒 + 計画 1 秒ほど（定数からの見積もり） |
| 過半数を失う | 書き込みは 503（最大 5 秒待ってから）。読み込みは手元の状態で返る。動いている仕事はそのまま動くが、配置し直しはされない | 回復すれば書き込みはすぐ戻る |
| 止まったノードが戻る | 次の周期で `node.up`。追いついたログから自分の仕事を突き合わせる。`prefer` の pin は、移ってから 30 秒が過ぎていれば戻る。縮退した仕事は冷却の後に戻る | `node.up` まで約 1 秒、戻しは冷却 30 秒の後 |
| 所有者のノードが止まる | 配信は途切れる。送り手がどれかのノードへつなぎ直すと、そのノードが新しい所有者になる。写しは新しい所有者から受け直す | 送り手のつなぎ直し次第 |
| 部屋を置いたノードが止まる | 部屋は別のノードへ移る。参加者は入り直す（どのノードにつないでも新しいノードへ `redirect` される。[10. 部屋](10-rooms.md)） | ノードの停止と同じ |
| ネットワークの分断 | 少数側のノードは、過半数（またはリーダー）との通信が 4 秒を超えて途絶えると**自らフェンシングします**: pull の入力・push の出力・パイプラインを停止し、`/readyz` を 503 にします（ログに `grid: fenced`）。多数側はハートビートの途絶（5 秒）の後にワークロードを移動するので、同じワークロードが 2 か所で動くことはありません。分断が解消すると、最新の割り当てに従って再開します（別のノードへ移動済みなら再開しません）。部屋と、ノードが直接受けている送り手（RTMP など）は停止しません | 停止まで約 4〜5 秒、解消後は数秒で再開 |

`tools/interop/cluster.sh` は 3 台で次を確かめる: どのノードからでも書ける、同じ名前は 409、1 台を止めても書ける、
戻ったノードが追いつく、過半数を失うと 503・読みはできる、全ノードが収束する、中継の写しから HLS・RTMP で見られる、
所有権の移動で古い送り手が切られる、入力が終わると所有権が解放される（他のノードの視聴は 404）、ハートビートの実測の使用量、
`prefer` の pin の移動と戻り（冷却の後）、`strict` の pin は移さない、分断（2 台を SIGSTOP）で少数側が pull の入力を停止して
`/readyz` が 503 になり、復旧すると 1 台で再開する、部屋の別ノードへのリダイレクトと移動、drain 中のノードが新しい RTMP の
送り手を拒否して WHIP を 307 でリダイレクトし、接続済みの送り手は継続する。平文の構成では、シークレットを知らない相手が
Raft と中継のチャレンジレスポンス認証で拒否されることも確かめます。`CLUSTER_TLS=1` では mTLS を使い、別の認証局の証明書を持つ
ノードが拒否されることを確かめます。
`tools/interop/grid_single.sh` は 1 台で、優先度による縮退・置けない理由・冷却の後の戻しを確かめる。
