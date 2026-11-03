# 13. 運用

ヘルスチェック、ログ、監視、イベントによる自動化、障害の後の回収、バックアップ、版の更新、容量の見積もり、
ノードの排出、終了のしかた。

- 関係するページ: [3. 設定](03-configuration.md)、[5. 録画](05-recording.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[12. セキュリティ](12-security.md)、[15. 管理 API と SDK](15-api-and-sdks.md)
- 設計の理由: [ADR-0005 録画の形式と索引](../../adr/0005-recording.md)、[ADR-0009 分散の制御面](../../adr/0009-cluster.md)、[ADR-0013 グリッド](../../adr/0013-grid.md)

## 13.1 起動と終了コード

| 終了コード | 意味 |
|---|---|
| 0 | 正常な終了（SIGTERM・SIGINT の後） |
| 1 | 起動の途中の失敗（`fatal: ...`。ファイルが読めない、待ち受けを開けない、制御状態のログが途中で壊れている、クラスタの合言葉が短いなど） |
| 2 | 設定の誤り（知らないオプション、検査に通らない値） |
| 70 | クラスタで Raft の保存に失敗した（書けないまま動かない） |

## 13.2 ヘルスチェック

| 経路 | 認証 | 応答 |
|---|---|---|
| `GET /healthz` | なし | HTTP が答えられれば常に `200 ok` |
| `GET /readyz` | なし | 準備ができていれば `200 ready`、それ以外は `503` |

- 単一ノードは、起動の処理（制御状態の読み込み、録画の回収、待ち受け）が済むと ready になる。
- クラスタでは、リーダーが決まり、自分の登録（と最初のノードなら初期化）が合意されてから ready になる。
- 終了の合図を受けると `/readyz` は 503 を返す。
- **ready は一度 ready になると、終了まで下がらない。** クラスタで過半数を失っても `/readyz` は 200 のまま
  （そのとき書き込みの API は 503）。排出（[13.9](#139-ノードの排出)）も ready に影響しない。

コンテナ向けに、curl の無いイメージでも使える確認の入口がある。

```sh
senca-node --healthcheck http://127.0.0.1:8080/readyz
```

- **最初の引数**に書く（他のオプションと混ぜない）。URL を省くと `http://127.0.0.1:8080/readyz`。
- 平文の `http://` だけ。`HTTP/1.1 200` が返れば終了コード 0、それ以外は 1、URL の形が違えば 2。
- Docker のイメージの `HEALTHCHECK` はこれを 10 秒ごとに呼ぶ。

## 13.3 ログ

### ノードのログ

- 既定: 標準エラーに `RFC 3339 の時刻 + 空白 + 本文`。
- `--log-json`: 標準出力に 1 行 1 JSON（`{"ts": "...", "node": "<ノード ID>", "msg": "..."}`）。

```
{"msg":"senca-node 0.0.0 started (node f1be5f27675d35f8dd72b88676be8754)","node":"f1be5f27675d35f8dd72b88676be8754","ts":"2026-11-03T01:11:38.915Z"}
```

`--log-json` でも、次は標準エラーに平文で出る: codec worker の行（`senca-codec-worker[PID]: sandbox: ...` など）、
単一ノードの最初の管理者トークンの作成の行、`codec worker unavailable: ...`、`fatal: ...`。
ログを集めるときは標準出力と標準エラーの両方を取る。

ログに出る主なもの: 待ち受けの開始、送り手の接続（`publisher attached`）と切断、録画の失敗、回収
（`recovery: ...`）、保持期限による削除（`retention: deleting recording ...`）、認証の失敗の理由と request_id、
入力・出力のつなぎ直しのエラー、クラスタ・グリッドの出来事（`grid: node ... is down` など）。
stream key・トークンの値は出さない。

### 監査ログ

`data_dir/audit.log` に、状態を変える API の操作を 1 行 1 JSON で追記する（形は [12. セキュリティ](12-security.md#1212-監査ログ)）。
ノードは書く度にファイルを開き直すので、`mv` で回して（logrotate なら `create` の方式）よい。ノードは回さないので、
大きさの管理は運用の側で行う。書けないときはノードのログに `audit log write failed: ...` が出る（操作は失敗しない）。

## 13.4 監視の API

すべて `node:admin` が要る（`capabilities` は認証だけ）。

| 経路 | 中身 |
|---|---|
| `GET /api/v1/nodes` | 登録されたノードの一覧: `id`・`name`・`draining`・`registered_at`・`labels`・`live_streams`（答えたノード自身の分だけ数える）・`self`・`relay`（自分の中継の状態） |
| `GET /api/v1/nodes/{id}` | 1 つのノード |
| `GET /api/v1/grid` | グリッドの様子: `scheduler`（配置を決めているノード。リーダーに聞いたときだけ）、ノードごとの `up`・`draining`・`roles`・`labels`・`capacity`・`reserved`・`gpus`（セッション数と予約）・`capture`・`last_heartbeat`・`workloads`、仕事ごとの `state`（`running`・`degraded`・`pending`・`unschedulable`）・`node`・`device`・`level`・`reason` |
| `GET /api/v1/devices` | このノードのコーデックの提供者（使えるか・理由）、サンドボックスの状態、キャプチャ機器 |
| `GET /api/v1/capabilities` | このノードの受信・出力・コーデック（`codecs.transcode` は読み込めたもの）・機能の一覧 |

stream ごとの様子は `GET /api/v1/streams/{id}` の `observed_state`（`live`・`waiting_input`・`stopped`）と `live`
（開始時刻・ビットレート・トラック）、入力・出力は `status`（`state`・`last_error`・接続回数）で見る。

Prometheus などのメトリクスの出口は今は無い。上の API を定期に読んで監視する。

## 13.5 イベント（SSE）による自動化

`GET /api/v1/events`（`stream:read`）は、合意されて適用された変更を Server-Sent Events で流す。

```sh
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/events
```

```
event: node.updated
id: 10
data: {"data":{"draining":true},"occurred_at":"2026-11-03T01:12:07.603Z","resource_id":"f1be5f27675d35f8dd72b88676be8754","type":"node.updated"}
```

- `id` は制御ログの位置。自分のテナントの出来事だけが届く。ノード・装置・グリッドの出来事（`node.*`・`device.*`・
  `grid.decisions`・`stream.owner`）は `node:admin` を持つときだけ。
- 15 秒ごとに keep-alive を送る。
- `Last-Event-ID` を付けてつなぎ直すと、その位置がノードの今の位置より古ければ、すぐに `resync` が届く
  （過去の出来事を再送はしない）。読むのが遅れて取りこぼしたときも `resync` を送って閉じる。
- 部屋の出来事（`room.started`・`room.connection.created` など）は `id` を持たず、取りこぼしても再送しない。

自動化の形: つないだとき・`resync` を受けたときに REST で一覧を読み直し、以後はイベントで差分を追う。
イベントの種類の一覧は [15. 管理 API と SDK](15-api-and-sdks.md)。

## 13.6 障害の後の回収

ノードは起動の度に録画を回収する（`recovery.rs`）。

1. 削除の途中だった録画の削除を終える。
2. `recordings/*/index.jsonl` を読み、`end` の行が無い（書きかけの）索引は、末尾の書きかけの行を切り捨て、
   `end`（`state: "interrupted"`、終わりの時刻は最後のセグメントの終わり）を追記して閉じる。制御状態の録画も
   `interrupted` にする。ログ: `recovery: recording <ID> closed as interrupted (<N> segments)`。
3. ここまでの後も制御状態で `recording` のままの録画（索引が無いものなど。このノードの分）を `interrupted` にする。
4. どの索引からも参照されていないセグメントを `segments/{stream}/orphans/{run}/` へ移す（消さない）。
   読めない索引が 1 つでもあれば、この隔離は行わない（参照を失わないため）。読めない索引そのものには触らない。
5. `--verify-recordings` があれば、索引にあるすべてのセグメントの SHA-256 を照合し、合わないものをログに出す。

`interrupted` の録画も、それまでのセグメントは VOD で再生・書き出しできる。

制御状態（単一ノードの `control/log`）は、末尾の書きかけのレコードを切り捨てて読む。途中のレコードが壊れていれば
起動しない（黙って捨てない）。

確かめたこと: 録画中に SIGKILL で止めて起動し直すと、録画は `interrupted` になり、それまでの 3 セグメントが残った。
SIGTERM で止めたときは `finalized` だった。

## 13.7 バックアップ

| 対象 | 重要度 | 内容 |
|---|---|---|
| `control/`（単一ノード: `log`・`snapshot.json`） | 必須 | 制御状態のすべて（stream・トークン・録画の記録・予約・入力・出力・秘密値・再生用の鍵） |
| `control/secret-key` | 必須 | 秘密値を開く鍵。失うと預けた秘密値は戻せない |
| `recordings/` と `segments/` | 録画を守るなら必須 | 索引とセグメントは**組で**取る（索引がセグメントを参照する） |
| `snapshots/` | 任意 | 定期の静止画 |
| `node-id` | 推奨 | ノードの ID。替わると別のノードとして登録される |
| `audit.log` | 方針次第 | 監査ログ |

- 一貫した写しを取るには、ノードを止めてから写すか、ファイルシステムのスナップショット（LVM・ZFS・btrfs など）を使う。
  動いているまま `control/` を写すと、写しの作り直し（`snapshot.json` を書いてログを空にする）と重なって
  変更を取りこぼすことがある。
- 戻すときは、ノードを止め、データ置き場をまとめて戻して起動する。書きかけの録画は起動時の回収で閉じられる。
- バックアップには再生用の鍵・封じた秘密値・トークンのハッシュが入る。データ置き場と同じように守る
  （[12. セキュリティ](12-security.md#128-データ置き場の保護)）。
- クラスタ: 制御状態は全ノードに複製されているが、録画はそれを録ったノードのディスクにだけある。録画は各ノードで取る。
  Raft の保存（`control/raft/`）を、動いているクラスタの 1 台に古い写しで戻さない（投票者は永続化した状態を
  失わない、という Raft の前提が崩れる）。クラスタの単位での戻し方は [9. クラスタとグリッド](09-cluster-grid.md)。

## 13.8 版の更新

SENCA は v0.0.0 で、版の間のデータの互換は約束していない。今の形について分かっていること:

- 制御状態のログは、コマンドを `op` の名前で表した JSON。制御状態の項目の多くは省略時に既定値で読む作りだが、
  古い版は知らない `op` を読めず、起動しない（`control log record N: ...`）。**版を下げることはできない**と考える。
- 録画の索引は `begin` の行に `schema`（今は 1）を持つ。知らない `schema` の索引は回収で触らず（`left untouched`）、
  そのときは孤立したセグメントの隔離も行わない。
- `senca-node` と `senca-codec-worker` は版付きのプロトコルで話す。**2 つは必ず同じ版にそろえて入れ替える。**
- クラスタで版の違うノードを混ぜた動作は試していない。

手順（単一ノード）:

1. バックアップを取る（[13.7](#137-バックアップ)）。
2. ノードを SIGTERM で止める（録画は `finalized` で閉じる）。
3. `senca-node`・`senca-codec-worker`（使うなら `senca-capture-worker`）を入れ替える。
4. 起動し、`/readyz` とログ（`recovery: ...`・`fatal: ...`）を確かめる。

## 13.9 ノードの排出

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/nodes/$NODE/drain     # 202
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/nodes/$NODE/undrain   # 202
```

- `node:admin` が要る。ノードの `draining` が立ち、イベント `node.updated` が出る。
- グリッドは、このノードに新しい仕事（pull 入力・push 出力・パイプライン・部屋・メディアパイプライン）を置かず、
  置いてある仕事を移せるものから他のノードへ移す（[9. クラスタとグリッド](09-cluster-grid.md)）。
- **排出は RTMP・SRT・WHIP の送り手や視聴者の接続を断らない。** `/readyz` も 200 のまま。ロードバランサーや
  DNS からは別に外す。送り手は切れた後、別のノードへつなぎ直してもらう。
- 単一ノードでは移す先が無いので、仕事はそのまま残る（確かめた: 排出の後も pull 入力は `running` のまま、`/readyz` は 200）。

装置（GPU・キャプチャ機器）だけを止めるときは `POST /api/v1/nodes/{node}/devices/{device}/disable`（戻すのは `enable`）。

## 13.10 終了のしかた

SIGTERM か SIGINT（`Ctrl-C`）で、ノードは次の順に終わる。

1. ログ `shutting down`。`/readyz` が 503 になる。
2. クラスタなら、ノード間の中継の接続に GoAway を送り、このノードが終わることを相手のノードに伝える。
3. stream ごとに、作りかけのセグメントを閉じ、録画中のものをすべて `finalized` で閉じる（索引に `end` を書いて fsync）。
4. codec worker を止める。
5. ログ `bye`、終了コード 0。

- 送り手・視聴者の接続はプロセスの終わりとともに切れる。
- 待つ時間の上限は持たない（試験では数 ms〜数十 ms で終わった）。Docker の `docker stop` は既定で 10 秒の後に
  SIGKILL を送る。SIGKILL・停電で止まったときは、次の起動で回収される（[13.6](#136-障害の後の回収)）。

## 13.11 容量の見積もり

### ノードの申告

| オプション | 既定 | 意味 |
|---|---|---|
| `--capacity-cpu-millis` | CPU の数 × 1000 × 90% | グリッドが仕事に貸す CPU（OS・制御面の取り分を残す） |
| `--capacity-memory-mb` | `MemTotal` の 85% | 仕事に貸すメモリ |
| `--net-mbps` | 10000 | 上り・下りそれぞれの帯域 |
| `--label KEY=VALUE` | なし | 配置の条件（`zone=a` など） |

GPU は codec worker が見つけた装置ごとに、符号化・復号のセッション数として申告される（符号化は上限が分からなければ 8、
復号は 16 以上）。申告は `GET /api/v1/grid` の `capacity`・`gpus` で見られる。

### グリッドが数えるもの

グリッドは、仕事ごとに次の**見積もり（予約）**を足して配置する（`crates/senca-control/src/workloads.rs`）。
観測した使用量ではない。

| 仕事 | CPU | メモリ | 帯域 |
|---|---|---|---|
| pull 入力 | 150 m | 64 MB | 下り 10 Mbit/s |
| キャプチャ入力 | 500 m | 64 MB | —（H.264/H.265 で符号化するなら GPU の符号化 1） |
| push 出力 | 100 m | 64 MB | 上り 10 Mbit/s |
| パイプライン | 部品の数から（復号 400 m、GPU の復号なら 100 m、合成・板書補正・拡大縮小・切り出し 300 m など） | 部品の数から | 上下 10 Mbit/s |
| 部屋（参加者の上限 N、既定 50） | 200 + 20N m | 64 + 2N MB | 上り 2N、下り 2N × min(N, 8) Mbit/s |

- **RTMP・SRT・WHIP の送り手（push の受信）と、HLS などの視聴者はグリッドの予約に入らない。** そのぶんは
  `--capacity-*`・`--net-mbps` を実機より小さく申告して残しておく。
- 部屋の既定（50 人）は下り 800 Mbit/s を予約する。`--net-mbps` を小さくすると、部屋が置けない
  （`unschedulable`）ことがある。部屋の `max_participants` を実際に合わせる。
- codec worker のプロセス数は `--codec-workers`（既定 CPU の数）。パイプラインは別に 1 本ごとに 1 プロセス。
- 録画の書き手のキューは `--record-queue`（[3. 設定](03-configuration.md)）。ディスクが遅いとキューがあふれ、
  録画に欠落（gap）が記録される（ライブは止まらない）。

### 測った数字

開発機 1 台（8 スレッド、ループバック）で、送り手 4 本 × 視聴者 200 人（各 約 4 Mbit/s）の HLS を 60 秒配り、
877 Mbit/s・セグメントの取得 p99 11.9 ms・誤り 0 だった（[performance.md](../../conformance/performance.md)）。
これは道具の動作と傾向の確認で、保証値ではない。基準環境での負荷試験と 72 時間の試験はまだ。
