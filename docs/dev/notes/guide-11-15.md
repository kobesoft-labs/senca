# ガイド 11〜15 章から移した開発者向けの情報 / Developer notes moved from guide chapters 11–15

利用者ガイドの 11〜15 章（[日本語](../../guide/ja/README.md)・[English](../../guide/en/README.md)）から、実装の場所、試験の内容、
検証の状況など、開発者向けの情報をこちらへ移しました。ガイドを書き換えるときは、ここも合わせて見直してください。

These implementer-oriented details (code locations, test contents, verification status) were moved here from chapters 11–15
of the user guide.

## 11. メディアパイプライン / Media pipelines

- API: `crates/senca-node/src/api/media_pipelines.rs`。部屋の配置先のノードで動く処理（1 秒ごとの突き合わせ）: `crates/senca-node/src/mediapipes.rs`。
  API handler in `api/media_pipelines.rs`; the per-node reconciler that runs every second is `mediapipes.rs`.
- 参加者一覧は `mediapipes.rs` の `participants()` が、そのノードの部屋の処理（`api_list_participants`）から取る。カスケードで別のノードにいる参加者の
  トラックは書き出さない（[10-rooms.md](../../guide/ja/10-rooms.md) の「メディアパイプラインで書き出せるのは、home のノードにいる参加者のトラック」）。
  以前のガイドは「ノードではカスケードは有効になっていない」と書いていたが、f5f99d9 でカスケードが入ったため改めた。
- 端から端までの試験: `tools/interop/node_media_pipelines.sh`（Chrome 2 ページ）。2 人の部屋で、参加者ごとのストリームが再符号化なしで HLS で復号できること、
  AV1 640×360 の合成、参加者ごと・合成の録画（3 本）と MP4 への書き出し、RTMP の送り出し、削除で合成のパイプラインが消えストリームが残ることを確かめる。
  End-to-end test: `tools/interop/node_media_pipelines.sh` (two Chrome pages) checks the points above.
- グリッドの見積もりの出所: `crates/senca-control/src/workloads.rs`。
- ADR-0016 にある S3 互換の保存先と WHIP での送り出しは、ノードに未実装。
  The S3-compatible storage and WHIP egress described in ADR-0016 are not implemented in the node.

## 12. セキュリティ / Security

### 独立したレビューを受けていないもの / Not yet independently reviewed

出荷 gate の G8（[RELEASE.md](../RELEASE.md)）は未了。次は、担当した実装者と独立したレビューの報告書が無い。

- 暗号プロトコル: TLS 1.3（HTTPS・RTMPS・クラスタの mTLS・外向きの HTTPS）、DTLS 1.2・SRTP（WebRTC）、SRT の暗号、QUIC
- 認可: JWT、署名付き URL、API トークン
- 分散の所有権: fencing、mTLS

あわせて、24 時間の fuzz（今は 60 秒の smoke のみ）と 72 時間の連続試験も未了（[適合台帳](../conformance/ledger.md)）。
TLS の実装は SENCA 自身のもの（暗号の演算は検証済みのライブラリ）で、まだ独立したレビューを受けていない。

Release gate G8 is open: no independent review report exists for the cryptographic protocols (TLS 1.3, DTLS 1.2/SRTP, SRT
encryption, QUIC), authorization (JWT, signed URLs, API tokens) or distributed ownership (fencing, mTLS). The 24-hour fuzzing
campaign and the 72-hour soak are also outstanding.

### 確かめたこと / What was verified

- JWT: Ed25519 の鍵で、テナント `acme` の JWT がストリームを作れ、`tenant:admin` の API は 403、そのストリームは `default` テナントから見えないことを確かめた。
- mTLS: ガイド 12.7 の証明書の作り方で 2 台のクラスタが組めることを確かめた（`tools/interop/cluster.sh` の `CLUSTER_TLS=1` と同じ形）。
- codec worker のサンドボックスは、GPU の実機での確認がまだ。/ The sandbox has not been verified on real GPUs.

### ガイドの記述を改めた点 / Corrections

- 12.10 は「ノードは認証 Webhook も匿名の参加も使わない」と書いていたが、f5f99d9 でノードに認証 Webhook（`--rooms-auth-webhook`・
  `--rooms-auth-mode`・部屋の `auth_webhook_url`）が入ったため、10 章に合わせて改めた。12.6 の外向きの接続の一覧にも部屋の Webhook を足した。

## 13. 運用 / Operations

- 起動時の録画の回収: `crates/senca-node/src/recovery.rs`。/ Recording recovery at startup lives in `recovery.rs`.
- 確かめたこと: 録画中に SIGKILL で止めて起動し直すと、録画は `interrupted` になり、それまでの 3 セグメントが残った。SIGTERM で止めたときは `finalized`。
- 終了の処理は、試験では数 ms〜数十 ms で終わった（待つ時間の上限は持たない）。/ Shutdown took a few to a few tens of ms in tests.
- クラスタで版の違うノードを混ぜた動作は試していない。ガイドでは「対応していない」と書いている。/ Mixed-version clusters have not been tested.
- グリッドの見積もりの出所: `crates/senca-control/src/workloads.rs`。
- 性能: 開発機 1 台での HLS の数字（[performance.md](../conformance/performance.md)）は道具の動作と傾向の確認。基準環境での負荷試験と 72 時間の試験はまだ。
- 13.11 の部屋の帯域は、以前の日本語版で上り・下りが逆だった。`workloads.rs` は `net_in_mbps: 2 * max`、`net_out_mbps: 2 * max * max.min(8)`
  なので、「受信 2N、送信 2N × min(N, 8)」「送信 800 Mbit/s」に直した（英語版は元から正しい）。

## 14. コーデックと特許 / Codecs and patents

- 実装: `crates/senca-codec`（提供者・dlopen）、`crates/senca-codec-worker`（worker・サンドボックス）、`crates/senca-node/src/codec.rs`。
- ハードウェアの経路は、開発機に GPU が無いため実機で未検証のものがある（各提供者のソースの「検証の状況」、REQ-004）。
  Some hardware paths have not been verified on real hardware (see each provider's source and REQ-004).
- ADR-0004 の表にある Apple VideoToolbox の提供者は未実装。/ The VideoToolbox provider listed in ADR-0004 is not implemented.
- 14.3 の 503 の例と 14.8 の `capabilities` の例は、GPU の無い開発機で実行して得たもの。

## 15. 管理 API と SDK / Management API and SDKs

- 実装: `crates/senca-node/src/api/`（振り分けと共通の契約は `api/mod.rs`）、`crates/senca-node/src/events.rs`（SSE）、
  `crates/senca-control/src/auth.rs`・`jwt.rs`・`problem.rs`。
- 契約の根拠: 仕様 SENCA-SRS-001 10 章、[REQ-002](../requirements/REQ-002-control-api-and-console.md)。仕様との差は [api-known-issues.md](api-known-issues.md)。
- 確かめ方: `tools/interop/openapi.sh` が (a) `api/mod.rs` の経路表・handler の権限と openapi.yaml の過不足、(b) 実際にノードを起動して全操作を送り、
  状態コードが文書にあり本文がスキーマに合うことを見る。
- 型の生成: JS の SDK は `apps/sdk-js/scripts/gen_types.py`（`npm run gen`）、Rust のクライアントは `crates/senca-client/gen/gen_models.py`
  （`src/models.rs` を作る）。どちらも openapi.yaml の説明文をコメントに写すので、openapi.yaml の説明を変えたら作り直す。
- JS の SDK の試験:

  ```
  cd apps/sdk-js && npm test        # 単体（偽の WebSocket・RTCPeerConnection・fetch、オフライン）
  npm run build                     # tsc で型を検査し、types/・API.md を作る（TypeScript 5.x）
  npm run gen                       # openapi.yaml が変わったら型を作り直す
  tools/interop/sdk_js.sh           # 本物の senca-node と Docker の Chrome 3 ページまで
  ```

- Rust のクライアントの試験: `cargo test -p senca-client`（`tests/node.rs` は本物の senca-node を起動して確かめる）。
- 15 章の部屋のイベントの配信先は、以前「部屋を置いたノードにつないだ購読者にだけ」と書いていたが、カスケードでは edge の参加者のイベントは edge のノードに流れる
  （10 章 §12）ため、「イベントが起きたノード」に改めた。
