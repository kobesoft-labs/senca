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
| `js/api/resources.js` | openapi.yaml の操作を 1 つずつ関数に |
| `js/api/sse.js` | `GET /events` を fetch のストリームで読む（`Last-Event-ID`・`resync`） |
| `js/api/pipelines.js` | パイプライン・制作の API（ノードに未実装。404 なら「このノードでは使えません」） |
| `js/player/` | MSE の HLS 再生（fMP4 だけ）: `m3u8.js`・`mp4.js`・`hls.js`・`widget.js` |
| `js/pipeline/model.js` | パイプライン定義（DAG）と検査。画面に依存しない |
| `js/views/` | 画面 |

## 再生（`js/player/hls.js`）の決まりと限界

- fMP4 の HLS だけ。初期化セグメントからコーデック文字列を作り、SourceBuffer に順に追加する。
- ライブは端の 3 本前から始め、遅れすぎたら端へ寄せる。不連続（送り手の再接続）は `timestampOffset` でつなぐ。
- 幅・高さが 0 の AV1 の初期化セグメント（今のノードの Enhanced RTMP 由来）は、キーフレームの sequence header から大きさを読んで補正する。
- MPEG-TS は扱わない（Safari のネイティブ再生へ。ほかのブラウザは URL を出して VLC などを案内）。
- LL-HLS の part・ABR（複数の variant）・暗号化・字幕は使わない。
