# 9. クラスターとグリッド

[English](../en/09-cluster-grid.md)

複数の senca-node を 1 つのクラスターにまとめると、制御状態（ストリームやトークンなどの設定）が全ノードに複製されます。
入力・出力・パイプライン・部屋は、**グリッド**の仕組みで各ノードへ配置されます。
ノードやデバイスが停止しても、ワークロードは稼働中の別のノードへ移るか、縮退した状態で動き続けます。

- 設計の背景: [ADR-0009 分散の制御プレーン](../../dev/adr/0009-cluster.md)、[ADR-0013 グリッド](../../dev/adr/0013-grid.md)、[ADR-0014 中継](../../dev/adr/0014-relay.md)
- 関連するページ: [3. 設定](03-configuration.md)、[6. 入力と出力](06-inputs-outputs.md)、[7. パイプライン](07-pipelines.md)、[10. リアルタイムの部屋](10-rooms.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)、[15. API と SDK](15-api-and-sdks.md)

グリッドは単一ノードでも同じ仕組みで動きます（そのノード自身がリーダーで、配置先もそのノードだけです）。
配置の指定（`placement`）、縮退、`GET /api/v1/grid` は単一ノードでも使えます。

## 9.1 全体の構成

| 構成要素 | 役割 |
|---|---|
| Raft | 制御状態へのすべての変更（コマンド）を全ノードに複製します。投票者は 1〜7 台です |
| 所有権 | 配信を受信したノードが、そのストリームの所有者になります（fencing token 付き） |
| 中継（ADR-0014） | 所有者でないノードで視聴するとき、所有者から AU を転送し、そのノード上のミラーから配信します |
| グリッド | リーダーがハートビートからノードの停止を検出し、毎秒ワークロードの配置を決めて合意ログに記録します。各ノードは自分に割り当てられた分だけを動かします |

複製されるのは**制御状態だけ**です。メディア（HLS のセグメントや録画のファイル）は、それを作ったノードのディスクにあり、
複製されません（[5. 録画](05-recording.md)）。

## 9.2 クラスターの構築

### 設定ファイル

クラスターの設定は JSON で書き、`--cluster-config FILE` で渡します（または `--config` の JSON の `cluster` に同じ内容を書きます）。
未知のフィールドがあると、起動時にエラーになります。

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
| `node_id` | はい | このノードの Raft の番号。1 以上で、`peers` に同じ `id` のエントリが必要です |
| `listen` | はい | ノード間の通信（Raft・提案の転送・ハートビート）の待ち受けアドレス |
| `peers` | はい | 投票するノードの一覧（自分を含む）。1〜7 台 |
| `peers[].id` | はい | Raft の番号 |
| `peers[].addr` | はい | そのノードの `listen` |
| `peers[].api` | いいえ | そのノードの管理 API の URL。307 リダイレクト、部屋の `redirect`、部屋の REST の転送に使います（下記） |
| `peers[].relay` | いいえ | そのノードの中継の待ち受けアドレス。既定は `addr` のポート + 1（下の検査を参照） |
| `secret_file` | はい | クラスターシークレットのファイル。全ノードで同じ内容にします。前後の空白を除いて 16 文字以上 |
| `tls_ca`・`tls_cert`・`tls_key` | いいえ | ノード間の mTLS。3 つすべてを指定するか、すべて省略します（一部だけ指定するとエラー） |
| `relay_listen` | いいえ | このノードの中継の待ち受けアドレス。既定は `listen` のポート + 1（下の検査を参照） |

次の場合、ノードは起動しません（終了コード 2 または 1）。

- `node_id` が 0、または `peers` に自分が含まれていない。`peers[].id` が 0 か、重複している。
- `peers` が 0 台か、8 台以上。
- クラスターシークレットが 16 文字未満。
- `tls_*` の一部だけを指定している。
- **アドレスが重複している**: いずれかのノードの中継アドレス（`peers[].relay`、省略時は `addr` + 1）が、別のノードの
  Raft のアドレス（`peers[].addr`）または中継アドレスと同じ。同じホストで Raft のポートを連番（7000・7001・7002）にすると、
  既定の中継ポート（+ 1）が隣のノードの Raft のポートと重なります。その場合は `relay_listen` と `peers[].relay` を明示してください。
- `listen` または `relay_listen` で待ち受けできない（ポートが使用中など）。

注意:

- **すべてのノードが投票者になります。** 投票しないメディア専用のノードは現在は作れないため、クラスターは最大 7 台です。
- **投票者の構成は設定ファイルで固定です。** 稼働中に投票者を追加・削除する API はありません。新しいクラスターは、すべての投票者が
  同じ `peers` で初めて起動したとき（データディレクトリの `control/raft/` が空のとき）に作られます。
- `peers[].api` には、**利用者の端末と他のノードの両方から到達できる URL** を書いてください。この値はノードの登録時（初回の起動時）に記録され、
  後で設定を変えても登録済みの値は変わりません。書かなければ、307 リダイレクトと、部屋の別ノードへの案内ができません。
- 単一ノードで使っていたデータディレクトリをそのままクラスターで使っても、制御状態は引き継がれません（クラスターでは Raft のログだけを使い、
  単一ノードのログ `control/log` は読み込みません）。

### ポート

| ポート | 接続元 | 用途 |
|---|---|---|
| `listen` | 他のノードのみ | Raft・提案の転送・グリッドのハートビート |
| `relay_listen`（既定 `listen` + 1） | 他のノードのみ | 中継（AU の転送） |
| `--http`・`--https` | 利用者と他のノード | 管理 API・HLS・シグナリング。`peers[].api` の宛先 |
| `--rtmp`・`--srt`・`--rtsp`・`--webrtc-udp` など | 利用者 | 配信と視聴（[4. 取り込みと再生](04-ingest-playback.md)） |

### クラスターシークレットと mTLS

ノード間の接続（Raft と中継）では、接続の直後にクラスターシークレットを使った**相互のチャレンジレスポンス認証**を行います。
双方が新しい乱数を送り、シークレットから導出した鍵による HMAC（双方の乱数とノード番号を含む）で、シークレットを知っていることを証明します。
シークレットそのものや固定の値は送らないため、通信を盗聴して再送しても接続できません。
相手のノード番号が接続先と違う場合、MAC が一致しない場合、5 秒以内に認証が終わらない場合は、接続を閉じます
（ログに `cluster: rejected connection from ...`・`relay: connection from ... refused` と出力されます）。

`tls_*` を設定すると、その上で mTLS を使います。クラスターの認証局（`tls_ca`）が発行した証明書を双方が検証します。
証明書の SAN には **`senca-cluster`**（DNS 名）が必要です（チャレンジレスポンス認証は TLS の中でも行います）。
`tls_*` を設定しない場合は平文の TCP になり、複製される制御状態と中継のメディアは暗号化されません（[12. セキュリティ](12-security.md)）。
中継の接続も同じ証明書を使います。

**バージョンの混在について**: チャレンジレスポンス認証はこのバージョンで導入されました。以前のバージョンのノード（固定のハッシュを
送る方式）とは接続できないため、クラスターのノードはまとめて更新してください。

証明書とシークレットの作成例（P-256。各ノードの証明書には serverAuth と clientAuth の両方を付けます）:

```sh
# クラスターの認証局
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
# クラスターシークレット（全ノードで同じファイル）
openssl rand -hex 32 > cluster-secret
```

別の認証局の証明書を持つノードは、クラスターシークレットを知っていても接続を拒否されます（ログに `mTLS from ... refused`）。

### 起動

```sh
senca-node --data-dir /var/lib/senca --node-name n1 \
  --http 0.0.0.0:8080 --public-http-url https://n1.example.com \
  --cluster-config /etc/senca/cluster-n1.json \
  --label zone=a --net-mbps 10000
```

- `--node-name` はノードの名前です。配置の指定や API で、ノードを名前で指定できます。ノードの ID（`data_dir/node-id` に保存される
  128 ビットの ID）とは別のものです。
- 起動してリーダーが決まると、各ノードが自分をクラスターに登録します。Raft の番号が最小のノード（例では `id: 1`）だけが、再生 URL の
  鍵と最初の管理者トークンを作成します。トークンはそのノードの `data_dir/control/bootstrap-admin-token`（mode 0600）に書き出され、
  複製されるので、どのノードでも使えます。
- 初期化が済むまで、`/readyz` は 200 になりません。その後も、過半数（またはリーダー）との通信が 4 秒を超えて途絶えている
  間は 503 を返します（[13. 運用](13-operations.md)）。
- シークレットを暗号化するクラスター共通の鍵がまだ無い場合は、各ノードが自分の鍵を暗号化して提案し、最初に適用されたものが
  使われます（[12. セキュリティ](12-security.md)）。

### 書き込みと読み込み

- **API はどのノードに送ってもかまいません。** 書き込みはリーダーへ転送され、合意の後に全ノードで同じ順序で適用されます。
- 合意の前に、そのノードの手元の状態で試しに適用するため、明らかな誤り（400・404・409 など）はすぐに返ります。最終的な結果は、合意後の適用で決まります。
- リーダーが決まっていない、過半数と通信できない、5 秒以内に適用されない、といった場合は
  **503 `CAPACITY_UNAVAILABLE`（`retryable: true`）** を返します。過半数を失った側では書き込めません。
- **読み込みは、そのノードの手元の状態から**返します（リーダーには問い合わせません）。書き込んだ直後の数 ms は、古い値が返ることがあります。
  過半数を失ったノードでも読み込みはできます。
- Raft の時間の設定: tick は 100 ms、選挙のタイムアウトは 1〜2 秒（10〜20 tick の乱数）、1000 件ごとにスナップショットを取ります。
  ログは `data_dir/control/raft/` に fsync してから送信・適用します。書き込めない場合、ノードは終了します（終了コード 70）。

### 複製されるもの

制御状態のすべてが複製されます: ストリーム、API トークン（ハッシュ）、録画の記録（メタデータ）、予約、Idempotency-Key の記録、
ノードの登録情報（`GET /nodes`）、入力・出力、シークレット（暗号化した値）とシークレットの鍵（クラスターシークレットから導出した鍵で暗号化したもの）、
ストリームの所有者、再生 URL の鍵、グリッドの状態（ノードの登録・ワークロード・割り当て・配置できない理由）、部屋、メディアパイプライン。

複製されないもの: メディアのデータ（ライブのセグメント・録画のファイル）、グリッドのハートビート、部屋の参加者とイベント
（部屋を配置したノードの手元にだけあります）。

## 9.3 ストリームの所有権と、どのノードでも視聴できる仕組み

- 配信者（RTMP・SRT・WHIP など、または pull の入力）が接続したノードは、そのストリームの所有権の取得をクラスターに提案します。
  提案が適用されると新しい fencing token が発行され、そのノードが所有者になります（SSE の `stream.owner`）。
- 他のノードはこの適用を見て、同じストリームの配信者を手元で受信していれば切断します（ログに `publisher evicted`）。
  同じストリームが 2 つのノードで同時に公開されることはありません。後から接続した方が優先されます。
- 配信者はどのノードに接続してもかまいません。別のノードで配信を受信し直すと、所有者はそのノードに移ります。
- **入力が終わると所有権を解放します。** 配信者が切断し、再接続の猶予（`--reconnect-grace`）が過ぎると、所有者のノードは
  所有権の解放を提案します（token が現在のものと同じ場合だけ解放されます。ログに `released ownership of stream ...`）。
  それ以降、他のノードの視聴者は停止した所有者へリダイレクトされず、404 になります。
- **停止と判定されたノードの所有権も解放します。** ノードの停止が記録されると、そのノードが持っていたストリームの所有者の記録を
  削除します。ノードが復帰して配信を受信し続けていれば、そのノードが所有権を取り直します（ログに `re-claimed stream ...`）。

### 視聴: 中継によるミラー

所有者でないノードに視聴のリクエストが来ると（署名付きの HLS、RTMP・SRT・RTSP の play、WHEP、パイプラインの入力）、そのノードは
所有者から中継で AU を受信し、手元にストリームの**ミラー**を作って配信します。

- 同じストリームを何人が視聴しても、ノード間で転送するのは 1 本だけです。ミラーのパッケージャーや GOP キャッシュは、所有者と同じように動作します。
- 転送するのはエンコード済みの AU だけです（変換はしません）。
- ミラーは、録画・字幕の作成・所有権の取得を行いません（これらは所有者のノードが行います）。
- 作成したばかりのミラーは、最初の再生リストができるまで最大 6 秒待ちます。
- 使われなくなって 30 秒たったミラーは削除します。所有権がこのノードへ移った場合は、ミラーを止めて実際の配信者に切り替えます。
- 所有者が別のノードへ移ると、ミラーは新しい所有者から受信し直します。その間、HLS の再生リストには不連続（discontinuity）が入ります。

### 307 リダイレクト（予備の経路）

- ライブ: ミラーを作れない、または 6 秒以内に再生リストができない場合で、手元に再生リストが無ければ、所有者のノードの
  `peers[].api` の同じパス（`/s/...`）へ **307** でリダイレクトします。
- 録画（VOD）: 録画のファイルは、録画したノードにあります。手元に無ければ、録画したノードへ 307 でリダイレクトします。
- リダイレクト先の URL は、登録時の `peers[].api` です。書いていなければリダイレクトできず、404 などになります。

中継の状態は、`GET /api/v1/nodes` の自ノード（`"self": true`）の `relay` で確認できます:
`peers[]`（`node`・`rtt_ms`・`tx_bps`・`rx_bps`・`tx_bytes`・`rx_bytes`・`tx_aus`・`rx_aus`・`overhead_per_au`・
`upstreams`・`downstreams`・`going_away`）と `mirrors`（ミラーしているストリームの ID の配列）。

## 9.4 グリッド

### ノードの登録と容量

各ノードは起動時に、ワークロードに割り当てられる資源をグリッドに登録します。その後も 30 ハートビートごとに見直し、変化があれば登録し直します。

| 資源 | 既定 | 変更方法 |
|---|---|---|
| CPU（ミリコア） | 論理 CPU 数 × 1000 × 90% | `--capacity-cpu-millis N` |
| メモリ（MB） | `/proc/meminfo` の MemTotal の 85%（読めなければ 4096 MB を基準） | `--capacity-memory-mb N` |
| 帯域（Mbit/s、送信・受信それぞれ） | 10000 | `--net-mbps N` |
| GPU | codec worker が検出したハードウェアのエンコーダー・デコーダー。デバイスごとに 1 つ（ID は `nvidia:0`・`intel-vpl:0`・`amd-amf:0`・`vaapi:0` の形式）。セッション数は取得できればその値、取得できなければエンコード 8・デコード 16 | — |
| キャプチャデバイス | capture worker が検出したデバイス | — |
| ラベル | `--label KEY=VALUE`（複数回指定可）。`senca.io/name` には `--node-name` の値が自動で入ります | `--label` |

- `--label` のキーと値は 1〜128 文字です。`senca.io/` で始まるキーは予約されています（コマンドラインでは拒否されます）。
- 設定ファイル（`--config`）では、`net_mbps`・`labels`・`capacity_cpu_millis`・`capacity_memory_mb` に同じ内容を書けます。
  設定ファイルの `labels` も `--label` と同じ規則で検査します（キーは 1〜128 文字、値は 128 文字まで、制御文字は使用不可、
  `senca.io/` で始まるキーと `raft_id` は予約、64 個まで）。規則に合わない場合は起動しません。
- ノードの役割には `media`（ワークロードを動かす）と `control`（投票者）があります。現在はすべてのノードが両方の役割を持ちます。
- **配置は予約量で計算します。** 予約量はワークロードごとの見積もり（9.5）の合計で、実際の使用量ではありません。
  ハートビートには実測の使用量も含まれます（CPU は `/proc/stat` の差分から求めたミリコア、メモリは `/proc/meminfo` の
  MemTotal − MemAvailable、帯域はループバックを除く `/proc/net/dev` の差分から求めた Mbit/s）。実測の使用量は、スケジューラー（リーダー）の
  `GET /api/v1/grid` の `usage` で確認できます（表示用で、配置には使いません）。

### ハートビートと停止の検出

- 各ノードは **1 秒ごと**にハートビートをリーダーへ送ります（クラスターの接続を使い、合意ログには記録しません）。ハートビートには、手元で受信している
  ストリーム、動かしているワークロード、デバイスの状態が含まれます。リーダーは、送信元のノードの Raft の番号と、登録された `raft_id` が一致するハートビートだけを受け付けます。
- リーダーは 1 秒ごとに確認し、最後のハートビートから **5 秒を超えた**ノードを停止と判定して合意ログに記録します。
  復帰すれば、復帰も記録します（SSE の `node.down`・`node.up`）。
- リーダーが交代すると、検出の状態を作り直します。作り直してから 5 秒間は、まだハートビートを受け取っていないノードも稼働中とみなします。
- デバイスの状態: 各ノードは 10 秒ごとにデバイスを検出し直します（新しい codec worker を 1 つ起動して対応機能を読み取り、
  capture worker にデバイスの一覧を問い合わせます）。登録済みのデバイスが検出されなくなるとハートビートで異常を伝え、
  リーダーがデバイスの停止（SSE の `device.down`）を、再び検出されると復帰（`device.up`）を記録します。異常なデバイス上のワークロードは、
  別のデバイスへ移るか縮退します。検出されなくなったデバイスも、登録からは削除しません（異常の状態のまま残ります）。
  新しく検出されたデバイスは、登録の見直し（30 ハートビートごと）で追加されます。これは手動での無効化（9.7 の `disable`）とは別のもので、
  デバイスの状態の検出結果によって手動の無効化が解除されることはありません。
- ノードの停止を記録した次の周期で、配置を計画し直します。ワークロードが追加・削除・変更されたときは、周期を待たずに計画します。

### ワークロード

グリッドで配置するワークロードは、リソースから自動で作られます（利用者が直接作るものではありません）。ワークロードの ID はリソースの種類と ID からなり、
`input:<id>`・`output:<id>`・`pipeline:<id>`・`room:<id>` の形式です。

| リソース | ワークロードになる条件 | 種類 |
|---|---|---|
| 入力（pull） | `enabled: true` | `ingest`（`protocol: "device"` は `capture`） |
| 出力（push） | `enabled: true` | `egress` |
| パイプライン | `desired_state: "running"` | `pipeline` |
| 部屋 | 常に | `room` |

ワークロードにならないもの: 配信者が接続する push の配信（接続したノードで受信します）、録画（所有者のノードで録画します）。
部屋のメディアパイプラインの書き出しは部屋の見積もりに加算され、合成は部屋と同じノードに固定したパイプラインになります
（[11. メディアパイプライン](11-media-pipelines.md)）。

各ノードは、割り当てのうち自分の分だけを動かします。割り当てが別のノードへ移ると token が変わり、元のノードは
その変更が適用された時点でワークロードを止めます。

### 配置の指定（`placement`）

入力・出力・パイプライン・部屋の作成時と PATCH 時に、`placement` を指定できます。結果は各リソースの `placement_status` に表示されます。

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
| `node` | なし | 手動の割り当て。ノードの ID か名前（`--node-name`）。保存時に ID に変換します。存在しないノードは 400 |
| `device` | なし | 手動の割り当て（GPU の ID）。`node` が無ければ、その GPU を持ついずれかのノードに配置します |
| `labels` | なし | ラベルの条件（すべてを満たすノードだけが対象）。最大 16 個 |
| `anti_affinity` | なし | グループ名。同じグループのワークロードは、同じノードに配置しません |
| `pin_policy` | `prefer` | 手動の割り当て先が停止したときの扱い。`prefer` は別のノードへ移し、割り当て先が復帰して冷却期間が過ぎたら戻します。`strict` は停止したままにします |
| `priority` | `normal` | `low` < `normal` < `high` < `critical` |
| `strategy` | `spread` | `spread` は配置後の空きが多いノードを、`binpack` は空きが少ないノードを選んで詰めます |
| `degrade` | なし | 縮退レベル（**パイプライン専用**。他のリソースに指定すると 400）。最大 8 レベル |

ラベルの条件:

| `op` | 形式 | 一致するノード |
|---|---|---|
| `eq` | `{"op":"eq","key":K,"value":V}` | ラベル K の値が V |
| `in` | `{"op":"in","key":K,"values":[...]}` | ラベル K の値が配列のいずれか（K が無いノードは一致しません） |
| `not_in` | `{"op":"not_in","key":K,"values":[...]}` | ラベル K の値が配列のどれでもない（K が無いノードも一致します） |

検査（違反は 400 `INVALID_CONFIG`）: `node`・`device`・`anti_affinity`・ラベルのキーと値は 1〜128 文字で、制御文字を含まないこと。
`in`・`not_in` の `values` は 1〜32 個。未知のフィールドは 400 です。

`degrade` のレベル:

- `label` は 1〜128 文字です。`full`（レベル 0 の名前）は使えず、重複もできません。
- `disable` は空にできず、グラフにある処理ノードの ID だけを指定できます。
- レベルは**累積します**（レベル 2 では、レベル 1 で外した処理ノードも外します）。外した処理ノードの下流は、入力がすべて外れれば外れます。
  入力の一部だけが外れる処理ノード（複数の入力を持つ sink・合成・音声のミックス）は、外れた入力を除いて残ります。
  詳しくは [7.7 縮退](07-pipelines.md) を参照してください。

手動の割り当て（pin）の動作:

- 割り当て先（ノード・GPU）が稼働している間は、`strict` も `prefer` もそこにだけ配置します（ドレイン中なら新しくは配置しません）。
- 割り当て先が停止すると、`strict` は配置できないまま（理由 `pin_target_down`）になり、`prefer` は別のノードへ移ります。
- `prefer` で別のノードへ移ったワークロードは、割り当て先が復帰し、移ってから 30 秒（冷却期間）が過ぎると戻ります（理由 `pin_returned`）。

### スコア

空いている資源に配置するときは、候補のノードのうちスコアが高い方を選びます（同点の場合は ID の小さいノード）。

- `spread`: 配置後の空きの割合（千分率。CPU・メモリ・帯域・GPU のセッションのうち最も小さいもの）。`binpack` では 1000 からこの値を引いたもの。
- データの局所性: ワークロードの入力のストリーム（出力ではそのストリーム、パイプラインでは `source` のストリーム）を受信しているノードに +10000。

## 9.5 ワークロードごとの見積もり

予約量は、ワークロードの種類ごとの固定の見積もりで計算します。CPU はミリコア、帯域は Mbit/s です。

| ワークロード | CPU | メモリ | 受信 | 送信 | その他 |
|---|---|---|---|---|---|
| 入力（pull） | 150 | 64 MB | 10 | 0 | — |
| 入力（`protocol: "device"`） | 500 | 64 MB | 0 | 0 | キャプチャデバイスを専有。`options.codec` が H.264/H.265 なら GPU のエンコードセッション 1 |
| 出力 | 100 | 64 MB | 0 | 10 | 入力のストリームを受信しているノードを優先 |
| 部屋（参加者の上限 N） | 200 + 20N | 64 + 2N MB | 2N | 2N × min(N, 8) | 動作中のメディアパイプラインごとに CPU + 20N、メモリ + 8N MB |
| パイプライン | 下の表の合計 | 下の表の合計 | 10 | 10 | GPU のセッションはすべて同じデバイスに配置 |

部屋の N は `settings.max_participants` です（指定が無ければ 50。1〜10000 の範囲に丸めます）。

パイプラインの見積もり（基本の CPU 200・メモリ 128 MB に、処理ノードごとに加算）:

| 処理ノード | CPU | メモリ | GPU |
|---|---|---|---|
| `encode`（H.264・H.265） | +100 | +128 MB | エンコード +1 |
| `encode`（Opus） | +50 | +128 MB | — |
| `encode`（AV1・VP8・VP9 などソフトウェアの映像） | +1500 | +128 MB | — |
| `decode`（入力の `source` のコーデックが H.264・H.265） | +100 | +128 MB | デコード +1 |
| `decode`（その他） | +400 | +128 MB | — |
| `compose`・`board`・`scale`・`crop` | +300 | +64 MB | — |
| `audio_mix` | +50 | — | — |

例: VP9 をデコードし、AV1 を 2 本エンコードし、そのうち 1 本は `scale` を通すグラフは、200 + 400 + 1500 × 2 + 300 = 3900 ミリコアです。
`scale` を外すレベル（その下流のエンコードも外れる）では 2100 ミリコアになります。

H.264・H.265 にはソフトウェアの実装がありません（[14. コーデックと特許](14-codecs-and-patents.md)）。そのため、これらを使う
パイプラインは GPU の無いノードには配置できません（理由 `no_device`）。

## 9.6 縮退・追い出し・復帰

スケジューラーはリーダーで毎秒動き、同じ入力に対しては常に同じ決定を出します。各周期の処理は次のとおりです。

1. **現在の割り当ての検査**: ノードの停止、デバイスの故障、条件の変更、アンチアフィニティによって条件に合わなくなったものを、再配置の対象にします。条件に合うものは動かしません。
2. **容量超過の解消**: ノードの容量が減って予約量が容量を超えた場合は、優先度の低い順に縮退レベルを下げ、それでも足りなければ追い出します（理由 `overcommitted`）。
3. **優先度の高い順**（critical → high → normal → low）に、再配置の対象と新しいワークロードを配置します。各レベル（full から順に）で、次の順に試します。
   1. 空いている資源に配置する。
   2. **より低い**優先度のワークロードの縮退レベルを下げる。
   3. **より低い**優先度のワークロードを追い出す。
   4. どれもできなければ、自分の次のレベル（縮退）で同じことを試す。

   縮退・追い出しの案は、追い出す数 → 追い出すワークロードの優先度の合計 → 下げるレベルの数、の順に小さいものを選びます。
   **同じか高い優先度のワークロードを押しのけることはありません**（配置できなければ `unschedulable`、理由 `contended`）。
4. 縮退しているワークロードは、レベルが変わってから **30 秒（冷却期間）** が過ぎたら元のレベルへ戻します（理由 `capacity_returned`）。
   試す順は、同じノードの空き → 別のノードの空きへの移動 → 同じノードで低い優先度のワークロードを縮退・追い出し、です。
5. **任意の移動**: ドレイン中のノードのワークロードと、`prefer` の割り当て先が復帰したワークロードを移します（1 回の計画で 64 件まで）。

スケジューラーの定数（設定では変更できません）: 復帰の冷却期間は 30 秒、復帰後に求める余裕は 0（余裕を求めない）、
1 回の計画での任意の移動は 64 件まで、データの局所性の加点は 10000 です。

レベルを下げたり追い出したりするのは、高い優先度のワークロードのため、またはノードやデバイスの故障・容量の減少のときだけです。
レベルを上げるのは冷却期間が過ぎた後だけです（状態が行ったり来たりするのを防ぐため）。

縮退したパイプラインは、そのレベルで外す処理ノードをグラフから除いて起動し直します（ログに
`pipeline <id>: running degraded (level N, without [...])`）。縮退レベルを持たないワークロード（入力・出力・部屋）は、
資源が足りなければ配置できないか、追い出されるだけです。

### `placement_status`

```json
{"state": "degraded", "node": "<node-id>", "node_name": "n2", "device": null,
 "level": 1, "level_label": "single", "since": "2026-11-03T10:00:00Z",
 "reason": {"code": "preempted", "by": "pipeline:..."}}
```

| `state` | 意味 | その他のフィールド |
|---|---|---|
| `running` | full のレベルで動作中 | `node`・`node_name`・`device`・`level`・`level_label`・`since`・`reason` |
| `degraded` | 縮退したレベルで動作中 | 同上 |
| `pending` | 配置を待っている | `reasons`（null） |
| `unschedulable` | どのノードにも配置できない | `reasons` |
| `not_scheduled` | ワークロードではない（無効な入力・出力、停止したパイプライン） | — |

`reason.code`: `placed`・`no_capacity`・`node_down`（`node`）・`device_down`（`device`）・`draining`・
`constraint_changed`・`preempted`（`by`）・`overcommitted`・`capacity_returned`・`pin_returned`・`pin_target_down`。

`reasons` は、配置できない理由ごとのノードの数と例（最大 8 件）です: `[{"reason": "cpu", "nodes": 2, "examples": ["<node-id>", ...]}]`。
理由: `node_down`・`not_media`・`draining`・`pinned_elsewhere`・`pin_target_down`・`labels`・`anti_affinity`・
`no_device`・`capture`・`cpu`・`memory`・`net_in`・`net_out`・`gpu_encode`・`gpu_decode`・`gpu_memory`・
`contended`（単独なら収まるが、同じか高い優先度のワークロードで埋まっている）。

## 9.7 運用の操作

すべて `node:admin` の権限が必要です（`tenant:admin` には含まれません）。

| 操作 | API | 応答 |
|---|---|---|
| ノードの一覧・取得 | `GET /api/v1/nodes`・`GET /api/v1/nodes/{id}` | 200 |
| ドレイン | `POST /api/v1/nodes/{id}/drain` | 202 |
| ドレインの解除 | `POST /api/v1/nodes/{id}/undrain` | 202 |
| デバイスを手動で無効化 | `POST /api/v1/nodes/{node}/devices/{device}/disable` | 202 |
| デバイスを有効に戻す | `POST /api/v1/nodes/{node}/devices/{device}/enable` | 202 |
| グリッドの状態 | `GET /api/v1/grid` | 200 |

- **ドレイン**（drain）: そのノードに新しいワークロードを配置せず、配置済みのワークロードを空きのある別のノードへ移します（理由 `draining`。
  1 回の計画で 64 件まで）。そのノードに pin したワークロードは移しません。`{id}` はノードの ID です（名前は使えません）。
  ドレインを解除しても、移したワークロードは自動では戻りません（`prefer` の pin を除く）。
  ドレイン中のノードは**新しい配信者を拒否します**: RTMP の publish は `NetStream.Publish.Rejected`（説明 `this node is draining`）、
  SRT の publish はハンドシェイクで拒否（理由 1503 = unavailable）、WHIP の `POST /whip/{stream}` はドレイン中でない別の
  ノードの同じパスへ **307** でリダイレクトします（該当するノードが無ければ 503 と `Retry-After`）。接続済みの配信者と
  視聴者はそのまま継続します。
- **デバイスの無効化**: GPU やキャプチャデバイスを、検出した状態とは関係なく手動で無効にします。無効にしたデバイスのワークロードは、別のデバイスへ移るか縮退します。
  ハートビートやノードの登録の見直しでは有効に戻りません（手動で戻すまで無効のままです）。`{node}` は ID か名前です。存在しないデバイスは 404 です。
  応答は `{"node": "<id>", "device": "nvidia:0", "enabled": false}` です。
- ノードをグリッドから削除する API はありません。停止したままのノードは `up: false` の状態で残ります。

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

- `scheduler` は、問い合わせたノードがリーダー（スケジューラー）なら自分の ID、そうでなければ null です。
- `last_heartbeat` と `usage`（最後のハートビートに含まれる実測の使用量）は、リーダーだけが保持しています。リーダー以外に問い合わせると null です。
- `reserved` は予約量（割り当てたワークロードの見積もりの合計）です。実際の使用量ではありません。
- `workloads[].state` は `running`・`degraded`・`unschedulable`・`pending` のいずれかです。
- `capture[]` の `disabled` は、手動で無効にしたときだけ表示されます。

### SSE のイベント

`GET /api/v1/events`（[15. API と SDK](15-api-and-sdks.md)）に流れます。グリッドとノードのイベントはテナントに属さないため、
`node:admin` を持つトークンにだけ見えます。

| イベント | `resource_id` | `data` |
|---|---|---|
| `node.updated` | ノードの ID | `{"draining": bool}`（登録・ドレイン） |
| `node.down`・`node.up` | ノードの ID | `{}` |
| `device.down`・`device.up` | ノードの ID | `{"device": ...}`（検出した状態の変化） |
| `device.disabled`・`device.enabled` | ノードの ID | `{"device": ...}`（手動の操作） |
| `grid.decisions` | 空 | `{"decisions": [...]}` |
| `stream.owner` | ストリームの ID | `{"node": "<node-id>"}` |

`grid.decisions` の各要素は `type` で区別します: `assign`（`workload`・`node`・`device`・`level`・`token`・`reason`）、
`move`（さらに `from`）、`degrade`・`restore`（`workload`・`level`・`device`・`reason`）、`evict`（`workload`・`reason`）、
`unschedulable`（`workload`・`reasons`）。

## 9.8 例

pull の入力を n3 に配置し、n3 が停止したら別のノードへ移す:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  https://n1.example.com/api/v1/streams/$SID/inputs \
  -d '{"protocol":"rtmp_pull","url":"rtmp://src.example.com/live/key",
       "placement":{"node":"n3","pin_policy":"prefer","priority":"high"}}'
curl -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/inputs/$IID | jq .placement_status
```

zone が a か b のノードに、2 台のカメラの入力を別々のノードとして配置する:

```json
{"protocol":"srt_pull","url":"srt://cam1.example.com:9000",
 "placement":{"labels":[{"op":"in","key":"zone","values":["a","b"]}],"anti_affinity":"cameras"}}
```

資源が足りなければ縮退するパイプライン（`placement` の部分）:

```json
{"priority":"normal","degrade":[{"label":"single","disable":["sc"]}]}
```

ノードをメンテナンスに入れる:

```sh
NID=$(curl -s -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/grid \
  | jq -r '.nodes[] | select(.name=="n2") | .id')
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/drain
# メンテナンスの後
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/undrain
```

GPU を手動で無効にし、有効に戻す:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/disable
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/enable
```

## 9.9 障害時の動作と所要時間

所要時間は、上で説明した定数（ハートビート 1 秒、停止の判定 5 秒、選挙 1〜2 秒、冷却期間 30 秒）から決まります。

| 障害 | 動作 | 所要時間の目安 |
|---|---|---|
| リーダー以外のノードが停止 | ハートビートが 5 秒を超えて途絶えると `node.down`。次の周期でワークロードを移し（`move`、理由 `node_down`）、移動先のノードがワークロードを開始します。`strict` の pin は移しません | 停止から移動まで約 6〜9 秒（3 台構成での実測） |
| リーダーが停止 | 選挙の間、書き込みは 503（retryable）。新しいリーダーは検出の状態を作り直し、5 秒の猶予の後に停止したノードを `node.down` にします | 選挙 1〜2 秒 + 猶予 5 秒 + 計画 1 秒程度（定数からの見積もり） |
| 過半数を失う | 書き込みは 503（最大 5 秒待ってから）。読み込みは手元の状態から返します。動作中のワークロードはそのまま動きますが、再配置はされません | 回復すれば書き込みはすぐに戻ります |
| 停止したノードが復帰 | 次の周期で `node.up`。追いついたログをもとに、自分のワークロードを照合します。`prefer` の pin は、移ってから 30 秒が過ぎていれば戻ります。縮退したワークロードは冷却期間の後に元のレベルへ戻ります | `node.up` まで約 1 秒、元のレベルへの復帰は冷却期間 30 秒の後 |
| 所有者のノードが停止 | 配信が途切れます。配信者がいずれかのノードへ再接続すると、そのノードが新しい所有者になります。ミラーは新しい所有者から受信し直します | 配信者の再接続しだい |
| 部屋を配置したノードが停止 | 部屋は別のノードへ移ります。参加者は入り直します（どのノードに接続しても、新しいノードへ `redirect` されます。[10. リアルタイムの部屋](10-rooms.md)） | ノードの停止と同じ |
| ネットワークの分断 | 少数側のノードは、過半数（またはリーダー）との通信が 4 秒を超えて途絶えると**自らフェンシングします**: pull の入力・push の出力・パイプラインを停止し、`/readyz` を 503 にします（ログに `grid: fenced`）。多数側はハートビートの途絶（5 秒）の後にワークロードを移すので、同じワークロードが 2 か所で動くことはありません。分断が解消すると、最新の割り当てに従って再開します（別のノードへ移動済みなら再開しません）。部屋と、ノードが直接受信している配信者（RTMP など）は停止しません | 停止まで約 4〜5 秒、解消後は数秒で再開 |
