# senca-client のテスト / senca-client tests

```
CARGO_TARGET_DIR=... cargo test -p senca-client
```

## 日本語

- **単体テスト**: URL・problem・再送の間隔・SSE の解析・Patch の 3 状態・operationId の対応表。
- **`tests/mock.rs`**: スクリプトどおりに応答するサーバーを使い、次を確認します。503 での再送（Retry-After）、POST の同じキーでの再送、
  再送しないエラー、上限に達したときの打ち切り、カーソルによるページ送り、SSE の再接続（Last-Event-ID）と resync。
- **`tests/node.rs`**: 実際の senca-node を起動して確認します（`SENCA_NODE_BIN`。未指定なら同じ target の `debug/senca-node`、それもなければ
  ビルドします。ノードは `--webrtc-udp` 付きで起動します）。確認する内容は次のとおりです。
  - 作成、Idempotency の再送（同じ ID と `Idempotency-Replayed`、別の本文なら 409）、SSE の `stream.created`、`limit=1` でのページ送り
  - 取得、If-Match 付きの更新、古い ETag での 412、If-Match なしでの 428、未知のフィールドでの 400
  - 削除（古いバージョンでは 412、削除後は 404）、401、トークンの権限（403）、秘密値、ノード、グリッド
  - 部屋: 作成、`limit=1` での一覧、取得、If-Match 付きの更新と 412、参加トークン、参加者の一覧（空）、統計、削除後の 404

## English

- **Unit tests**: URLs, problems, retry intervals, SSE parsing, the three states of Patch fields, the operationId table.
- **`tests/mock.rs`**: a server that answers from a script, covering retries on 503 (Retry-After), POST retries with the same key, errors
  that are not retried, giving up at the limit, cursor pages, SSE reconnection (Last-Event-ID) and resync.
- **`tests/node.rs`**: starts a real senca-node (`SENCA_NODE_BIN`; otherwise `debug/senca-node` in the same target; otherwise it builds one;
  the node is started with `--webrtc-udp`) and checks:
  - creation, Idempotency resends (same ID and `Idempotency-Replayed`, 409 for a different body), SSE `stream.created`, paging with `limit=1`
  - get, update with If-Match, 412 for a stale ETag, 428 without If-Match, 400 for unknown fields
  - deletion (412 for an old version, then 404), 401, token scopes (403), secrets, nodes, the grid
  - rooms: create, list with `limit=1`, get, update with If-Match and 412, join tokens, the participant list (empty), stats, 404 after deletion
