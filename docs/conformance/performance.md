# 性能・長時間の測定記録（REQ-010 R10-6）

**ここの数字は開発機 1 台での小規模な測定であり、AC-12（1000 視聴・概算 4.2Gbps、仕様 13.1 の基準環境）・
AC-21（72 時間）の判定ではない。** 送り手（FFmpeg）・視聴者（Python）・ノードが同じ機械で動き、通信はループバック。
道具が動くこと、桁と傾向、資源が回収されることを確かめるための記録。手順は `docs/RELEASE.md`（G5・G6）。

## 測定環境

| 項目 | 値 |
|---|---|
| 機械 | 開発機（仕様 13.1 の基準環境ではない）。AMD Ryzen 7 255（8 スレッド）、メモリー 23 GiB、ループバック |
| OS | Linux 7.0.0-42-generic x86_64 |
| ノード | `senca-node`（release build: `[profile.release]` lto=thin・codegen-units=4）、sha256 `fca77837d4420cf4…`、commit `4b5b416` ＋作業中の変更 |
| ノードの設定 | `--codec-workers 2`、録画なし、HLS は既定（TS、ライブの窓は既定値） |
| 素材 | 仕様 13.1 の基本: 1920x1080 30fps H.264 4Mbps（`testsrc2`、libx264 veryfast・Bフレームなし・GOP 1 秒）＋ AAC 128kbps。FFmpeg 7.0.2 が `-re -stream_loop -1 -c copy` で RTMP 送信 |
| 視聴者 | `tools/loadgen/loadgen.py`（Python 3.14、4 プロセス）。live の端から 3 本手前で始め、playlist を target duration の半分ごとに読み直して新しい segment を keep-alive で取る |
| 日時 | 2026-11-02 |

## 負荷（churn なし）: 送り手 4 本 × 視聴者 200 人 × 60 秒

`PUBLISHERS=4 VIEWERS=200 PROCS=4 tools/loadgen/load.sh 60`（結果 `out/load/20261102T161819Z/`）

| 指標 | 値 |
|---|---|
| 配信量 | 6.58 GB / 60 秒 = **877 Mbit/s**（視聴者 200 人 × 約 4.1Mbit/s。ほぼ素材の bitrate どおり） |
| segment の取得時間（要求〜最後のバイト） | p50 **1.4 ms**、p95 **4.9 ms**、p99 **11.9 ms**、最大 22.1 ms（6063 本） |
| playlist の取得時間 | p50 0.2 ms、p95 1.1 ms、p99 3.7 ms（11583 回） |
| 最初の segment まで | p50 1.3 ms、p95 3.4 ms（接続から。HLS の準備＝送信開始から最初の segment まで 2.0 秒） |
| segment の長さより遅れた取得 | 0 |
| 誤り（HTTP・接続） | 0 |
| ノードの CPU | 平均 4.3%（1 コア＝100%）。8 スレッドの機械で、負荷の大半は視聴者側の Python |
| ノードの RSS | 開始前 8 MB → 負荷中の最大 137 MB（ライブの窓が埋まるまで増える）→ 負荷の後 39 MB |
| fd・socket・スレッド | 負荷前 18 / 5 / 10 → 負荷中 最大 222 / 209 / 14 → 負荷後 18 / 5 / 10（残りなし） |

所見: この規模ではノードの CPU に余裕があり、律速は同じ機械の視聴者（Python）とループバック。
AC-12 の 1000 視聴・4.2Gbps は、視聴者を別の機械（複数台）から出し、25GbE の基準環境で測る必要がある。

## 長時間（短い版）: 送り手 4 本 × 視聴者 100 人 × 180 秒、接続の churn あり

`PUBLISHERS=4 VIEWERS=100 CHURN=30 PUB_CHURN=45 SIZE=1920x1080 BITRATE=4000 PROCS=4 tools/soak/soak.sh 180`
（結果 `out/soak/20261102T162027Z/`。視聴の長さは平均 30 秒の指数分布で切断・再接続、送り手は平均 45 秒に 1 本をつなぎ直す）

| 指標 | 値 |
|---|---|
| 視聴の接続 | 633 回（100 人分の churn）、segment 10215 本、490 Mbit/s |
| segment の取得時間 | p50 0.9 ms、p95 3.8 ms、p99 7.5 ms。遅れ 0、誤り 0 |
| 送り手のつなぎ直し | 6 回（想定外の終了 0） |
| crash・panic | なし（プロセスが残り、ログに panic なし） |
| ノードの CPU | 平均 3.4% |
| RSS（10 秒ごと） | 8 MB → 50 秒で 114 MB → 130〜140 秒で約 190 MB に達し、以後 178〜184 MB で平ら → 負荷の後 129 MB |
| warm-up 60 秒（180 秒の 1/3）の判定 | 143 MB → 183 MB、**+27.5%（AC-21 の 5% を超える: FAIL）** |
| warm-up 120 秒での再計算（`loadgen.py analyze --warmup 120`） | 185 MB → 183 MB、**−1.0%（PASS）** |
| fd・socket（負荷の前 → 後） | 18 → 18、5 → 5（回収されている） |
| スレッド（負荷の前 → 後） | 10 → 11（tokio の blocking pool の待機スレッドと見られる。72 時間の試験で増え続けないことを確かめる） |

所見:

- RSS は churn を含めて 2 分あまりで平らになり、その後の増分は見られない。60 秒の warm-up では窓・接続の
  バッファが埋まる途中を「増分」と数えてしまうため FAIL になった。AC-21 の本番（72 時間）は warm-up を 10 分以上に
  とる（`soak.sh` の既定は `min(DURATION/3, 600)` 秒）。
- 負荷の後に RSS が 129 MB 残る（開始前 8 MB）。stream は API で消したあと。アロケーターが返していない分か、
  消した stream の窓の残りかは未確認。72 時間の試験で「負荷の後の RSS」が回を重ねて増えないかを見る。
- fd・socket は負荷の前の数に戻った（接続・送り手のつなぎ直しで漏れていない）。

## まだ測っていないこと

- AC-12（基準環境・1000 視聴・N0）、AC-13（遅延 N1）、AC-R10-01〜03（stream 数・worker 数・ノード数に対する伸び）。
- LL-HLS・WebRTC・SRT・RTMP play の視聴者の負荷（道具は HLS の視聴者だけ）。
- 録画あり（`--record`）・変換あり（codec worker）の負荷。
- 72 時間の soak（AC-21）。
