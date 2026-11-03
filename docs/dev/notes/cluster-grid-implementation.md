# クラスタとグリッドの実装メモ / Cluster and grid: implementation notes

利用者ガイド（[9. クラスタとグリッド](../../guide/ja/09-cluster-grid.md)）から、実装と試験に関する記述をこちらへ移しました。

These implementation and test details were moved here from the user guide ([9. Cluster and grid](../../guide/en/09-cluster-grid.md)).

## コードの場所 / Where the code lives

| 内容 / Topic | 場所 / Location |
|---|---|
| Raft | `crates/senca-raft` |
| グリッド（配置の計画）/ Grid (placement planning) | `crates/senca-grid`（`src/plan.rs`） |
| ワークロードの見積もりと縮退のグラフの刈り込み / Workload estimates and degraded-graph pruning | `crates/senca-control/src/workloads.rs`（`prune_graph`） |
| 中継 / Relay | `crates/senca-relay` |

## 複製されるもの / What is replicated

利用者ガイドの「制御状態のすべて」は `ControlState` の全体を指す。
"The whole control state" in the user guide means all of `ControlState`.

## Raft のコマンドとガイドの言い回し / Raft commands and the guide's wording

| コマンド / Command | ガイドでの書き方 / Wording in the guide |
|---|---|
| `ClaimStream` | 所有権の取得を提案する / proposes taking ownership |
| `ReleaseStream` | 所有権の解放を提案する / proposes releasing ownership |
| `NodeDown`・`NodeUp` | ノードの停止・復帰を記録する（SSE `node.down`・`node.up`）/ records the node as down or up |
| `DeviceDown`・`DeviceUp` | デバイスの停止・復帰を記録する（SSE `device.down`・`device.up`）/ records the device as down or up |

## スケジューラーの定数 / Scheduler constants (`PlanConfig`)

`PlanConfig` の既定値。設定からは変更できない。/ Defaults of `PlanConfig`; not configurable.

| フィールド / Field | 値 / Value |
|---|---|
| `restore_cooldown_ms` | 30000 |
| `restore_headroom_permille` | 0（戻した後の余裕を求めない / no headroom required after restoring） |
| `max_voluntary_moves` | 64 |
| `locality_bonus` | 10000 |

## テスト / Tests

- `tools/interop/cluster.sh`（3 台 / three nodes）: どのノードからでも書ける、同じ名前は 409、1 台を止めても書ける、
  戻ったノードが追いつく、過半数を失うと 503・読み込みはできる、全ノードが収束する、中継のミラーから HLS・RTMP で視聴できる、
  所有権の移動で古い配信者が切られる、入力が終わると所有権が解放される（他のノードの視聴は 404）、ハートビートの実測の使用量、
  `prefer` の pin の移動と復帰（冷却期間の後）、`strict` の pin は移さない、分断（2 台を SIGSTOP）で少数側が pull の入力を停止して
  `/readyz` が 503 になり、復旧すると 1 台だけで再開する、部屋の別ノードへのリダイレクトと移動、ドレイン中のノードが新しい RTMP の
  配信者を拒否し WHIP を 307 でリダイレクトし、接続済みの配信者は継続する。平文の構成では、シークレットを知らない相手が
  Raft と中継のチャレンジレスポンス認証で拒否されることも確かめる。`CLUSTER_TLS=1` では mTLS を使い、別の認証局の証明書を持つ
  ノードが拒否されることを確かめる。ガイドの「停止から移動まで約 6〜9 秒」はこのテストでの実測値。
  証明書の作り方の例も、このスクリプトの手順と同じ。
- `tools/interop/cluster.sh` checks writes through any node, 409 on duplicate names, writes with one node stopped, catch-up of a
  returning node, 503 without a majority while reads still work, convergence, HLS and RTMP playback through relay mirrors, eviction
  of the old publisher when ownership moves, ownership release when an input ends (viewers on other nodes get 404), measured usage in
  heartbeats, moving and returning a `prefer` pin, not moving a `strict` pin, a partition (two nodes stopped with SIGSTOP) where the
  minority stops its pull input and reports 503 on `/readyz` and the input runs on exactly one node after healing, room redirects and
  moves, and a draining node refusing new RTMP publishers and redirecting WHIP with 307 while connected publishers continue. In plain mode
  it checks that a peer without the cluster secret is refused; with `CLUSTER_TLS=1` it uses mTLS and checks that a node with a
  certificate from another CA is refused. The guide's "about 6 to 9 s from the stop to the move" was measured here, and the
  certificate example in the guide follows this script.
- `tools/interop/grid_single.sh`（1 台 / one node）: 優先度による縮退、配置できない理由、冷却期間の後の復帰。ガイド 9.5 の
  「`scale` を外すレベルは 2100 ミリコア」の例はこのテストの構成。
  Priority-based degradation, unschedulable reasons and restoring after the cooldown. The guide's 9.5 example
  ("a level that disables `scale` needs 2100 millicores") is this test's setup.
