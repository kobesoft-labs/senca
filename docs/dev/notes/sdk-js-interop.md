# SDK（apps/sdk-js）の相互接続テスト / SDK interop test

`tools/interop/sdk_js.sh` の部屋のテストでは、次の内容を確認します。

- 3 つのページ（alice はサイマルキャストで 3 レイヤーを公開）が SDK だけを使って参加し、互いの映像をデコードできること
  （`session.stats()` の framesDecoded で確認）。
- `#chat` のメッセージが、DataChannel 経由と宛先指定（シグナリング経由）の両方で往復すること。
- carol が alice のレイヤーを `r2` → `r0` と切り替えると、デコードされる映像の幅が 1280 から 320 に変わること。
- 各ページが順に `close()` すると、残っている参加者に `participant.left` が届き、最後に部屋が空になること。

The room test in `tools/interop/sdk_js.sh` checks the following:

- Three pages (alice publishes three simulcast layers) join using only the SDK and decode each other's video
  (framesDecoded from `session.stats()`).
- `#chat` messages round-trip both over the DataChannel and addressed to one participant (via signaling).
- When carol selects alice's layer `r2` -> `r0`, the decoded width changes from 1280 to 320.
- When each page calls `close()` in turn, the remaining participants receive `participant.left`, and the room ends up empty.
