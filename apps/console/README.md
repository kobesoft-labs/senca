# SENCA 管理画面（apps/console）

公開の管理 API（`/api/v1`、`docs/api/openapi.yaml`）だけを使う単一ページアプリ（REQ-002）。
依存パッケージもビルドも無い（素の ES modules と CSS）。ノードが `--console-dir apps/console` で `/console/` に配る。

```
senca-node --console-dir apps/console ...   # → http://<node>/console/
tools/interop/console.sh                    # 配信の形・構文・ロジック・ヘッドレス Chrome での操作と再生
```

ログインは API トークン（初回は `{data-dir}/control/bootstrap-admin-token`）。トークンは sessionStorage にだけ置く。

## 構成

| 場所 | 中身 |
|---|---|
| `index.html`・`css/app.css` | 外枠と見た目（明暗は `prefers-color-scheme`）。インラインのスクリプトは無い（CSP） |
| `js/main.js` | 経路（`#/streams` など）、ログイン状態、SSE の開始 |
| `js/api/client.js` | fetch の包み。problem+json を `ApiError` に、401 はログインへ |
| `js/api/resources.js` | 管理 API の操作を 1 つずつ関数に（配信・録画・予約・トークン・入力・出力・秘密値） |
| `js/api/sse.js` | `GET /events` を fetch のストリームで読む（`Last-Event-ID`・`resync`） |
| `js/api/pipelines.js` | パイプラインの API（`/pipelines`・validate・control）。検証の問題（problem の details.issues）の取り出し、status とメーターの読み分け |
| `js/player/` | MSE の HLS 再生（fMP4 だけ）: `m3u8.js`・`mp4.js`・`hls.js`、WebRTC の再生 `whep.js`（WHEP、recvonly）、`widget.js`（自動 = WHEP → だめなら HLS） |
| `js/pipeline/model.js` | senca-pipeline のグラフ JSON（version 1）と編集用のモデルの行き来・ポートの型・検査。画面に依存しない |
| `js/views/` | 画面。`pipelines.js`（グラフの編集・保存・検証・開始/停止）、`production.js`（スイッチャー: `/pipelines/{id}/control`、5 Hz の状態とメーター、WHEP のモニター）、`stream-io.js`（入力・出力・字幕・再生 URL）、`secrets.js`、`send.js`（このブラウザから WHIP で送る） |

## 再生（`js/player/hls.js`）の決まりと限界

- fMP4 の HLS だけ。初期化セグメントからコーデック文字列を作り、SourceBuffer に順に追加する。
- ライブは端の 3 本前から始め、遅れすぎたら端へ寄せる。不連続（送り手の再接続）は `timestampOffset` でつなぐ。
- 幅・高さが 0 の AV1 の初期化セグメント（今のノードの Enhanced RTMP 由来）は、キーフレームの sequence header から大きさを読んで補正する。
- MPEG-TS は扱わない（Safari のネイティブ再生へ。ほかのブラウザは URL を出して VLC などを案内）。
- LL-HLS の part・ABR（複数の variant）・暗号化・字幕は使わない。

## パイプラインとスイッチャー

- グラフは `crates/senca-pipeline/src/model.rs` の JSON（知らない項目は拒否される）。部品の位置はグラフに入れられないので、
  このブラウザの localStorage（`senca.console.pipeline-layout.v1`、パイプラインの id ごと）に置く。保存前の下書きも localStorage。
- 保存 = `POST /pipelines`（止めた状態で作る）か `PATCH`（If-Match）。検証 = `POST /pipelines/{id}/validate` に編集中のグラフを送る
  （保存前のグラフも検証できる）。問題は problem+json の `details.issues`（`{code, node, message}`）を一覧にし、部品を赤くする。
- 開始・停止 = `PATCH {desired_state}`。スイッチャーは CUT・TAKE・FADE を `take`（トランジション付き）、シーンのクリックを
  `set_preview`、ダブルクリックを `switch_program` に。T バーは途中の位置を送れないので、端まで動かした時間のトランジションで take。
- ノードは status（`event: status`）とメーター（`event: meters`）を同じ `status` 欄に入れることがあるので、`event` で読み分ける。

## WebRTC

- 再生（WHEP）: `playback-url` の `playback.whep`（署名付き）へ offer を POST、201 の answer を入れる。止めると Location を DELETE。
  配信前・運べないコーデック（AAC だけ・H.265）は 422 → 自動なら HLS に戻す。
- 送信（WHIP、`#/send`）: カメラ・マイクか画面 → `ingest.whip_url` へ Bearer の stream key で POST。鍵は保存しない。
