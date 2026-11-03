# 1. 概要

SENCA は、ライブ配信・録画・リアルタイム通信のためのメディアサーバー（Rust、Apache-2.0）。
1 台の `senca-node` でも、複数のノードを束ねたクラスタ（グリッド）でも動く。

- 関係するページ: [2. クイックスタート](02-quickstart.md)、[3. 設定](03-configuration.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
- 設計の理由: [ADR-0001 全体構成](../../dev/adr/0001-architecture.md)

> **状態: v0.0.0（実験的）。** 機能はそろってきたが、出荷 gate（24 時間の fuzz、72 時間の連続試験、暗号・認可の
> 独立レビュー、署名付きの配布）をまだ通していない。本番には使わないこと（[1.5 状態](#15-状態何を確かめたか)）。

## 1.1 実行ファイル

| 実行ファイル | 役割 |
|---|---|
| `senca-node` | 本体。受信（gateway）・メディア処理・配信（origin）・制御（control）を 1 プロセスに同居させる |
| `senca-codec-worker` | コーデックを動かす別プロセス。`senca-node` がプールとして起動し、パイプ（大きなフレームは共有メモリ）でつなぐ。サンドボックスの中で動く |
| `senca-capture-worker` | キャプチャ機器（V4L2・ALSA・DeckLink）を扱う別プロセス。無ければキャプチャ機器が使えないだけ |

`senca-node` は、同じディレクトリにある `senca-codec-worker`・`senca-capture-worker` を探して起動する
（codec worker は `--codec-worker-bin` で場所を変えられる）。

## 1.2 構成

```
            RTMP/RTMPS  SRT  WHIP  RTSP/RTMP/SRT/HLS pull  キャプチャ
                 │       │     │            │                 │
                 ▼       ▼     ▼            ▼                 ▼
          ┌──────────────────────────────────────────────────────────┐
          │ stream hub（stream ごとに 1 タスク、有界キュー）            │
          │   ├─ packager → LiveCache（メモリ）→ HTTP: HLS（TS/fMP4/LL-HLS）
          │   ├─ 購読者: RTMP play・WHEP・RTSP play・push 出力・パイプライン
          │   └─ recorder → segments/（不変ファイル）＋ recordings/{id}/index.jsonl
          └──────────────────────────────────────────────────────────┘
                 │ 符号化・復号・合成                    ▲
                 ▼                                       │
          senca-codec-worker（別プロセス・サンドボックス）     │
                                                         │
  制御 API（REST・SSE）→ 制御状態（決定的な状態機械）──────┘
        単一ノード: control/log（追記・fsync）／分散: Raft（control/raft/）
        グリッド: 仕事（入力・出力・パイプライン・部屋）をノードへ配置
        中継: ノード間で符号化済みの AU を運ぶ
```

### 制御面（control plane）

- 制御状態（stream・トークン・録画の記録・予約・入力・出力・秘密値・パイプライン・部屋など）は、
  コマンドを適用する決定的な状態機械。状態を変える入口は 1 つ（`App::propose`）。
- 単一ノードでは、適用したコマンドを `control/log` に 1 件ずつ追記して fsync してから応答する。1000 件ごとに
  `control/snapshot.json` に写しを作り、ログを空にする。
- クラスタでは、同じコマンドを自前の Raft（`senca-raft`）で複製する。投票者は 1〜7 台。どのノードに書いても
  リーダーへ転送される。過半数を失った側は書けない（503）。読みはそのノードの手元の状態から。
  詳しくは [ADR-0009](../../dev/adr/0009-cluster.md) と [9. クラスタとグリッド](09-cluster-grid.md)。

### stream hub・packager・recorder

- **hub**（[ADR-0006](../../dev/adr/0006-stream-hub.md)）: stream ごとに 1 つのタスク。接続より長生きし、送り手の
  再接続を猶予（`--reconnect-grace`、既定 30 秒）の間は同じ stream として続ける。入口は有界のキューで、
  hub が遅れると送り手の読み込みが止まる（TCP の背圧）。
- **packager**: hub の中でセグメント分割と多重化を 1 回だけ行い、結果をメモリの `LiveCache` に置く。
  ライブの HLS はディスクを通らない。
- **recorder**（[ADR-0005](../../dev/adr/0005-recording.md)）: 誰かが録画しているときだけ、ライブと同じセグメントを
  `segments/` に不変ファイルとして書き、録画ごとの索引（`recordings/{id}/index.jsonl`）に追記する。
  キューがあふれたら録画の側で落として欠落（gap）を記録する（ライブは止めない）。
  起動時には書きかけの録画を `interrupted` として閉じる（[13. 運用](13-operations.md)）。

### codec worker のサンドボックス

H.264/H.265 の符号化・復号は GPU ベンダーの SDK（NVENC/NVDEC・oneVPL・AMF・VA-API）、AV1・VP8/VP9・Opus は
ロイヤリティフリーのライブラリ（dav1d・SVT-AV1・libvpx・libopus）で行う。どれも実行時に `dlopen` で読み込み、
`senca-node` の中には読み込まない。読み込むのは `senca-codec-worker` で、seccomp・Landlock・rlimit などで制限した
別プロセスとして動く（[12. セキュリティ](12-security.md)、[ADR-0004](../../dev/adr/0004-codecs-patents.md)、
[14. コーデックと特許](14-codecs-and-patents.md)）。
worker が無い・ライブラリが無いときは、受けたものをそのまま配る・録るのはでき、変換・静止画・サムネイルは
`503 CAPACITY_UNAVAILABLE` になる。

### グリッド

ノードの資源（CPU・メモリ・帯域・GPU・キャプチャ機器）を数えて、仕事（pull 入力・push 出力・パイプライン・
部屋・メディアパイプライン）をノードへ置く。配置は Raft のリーダーが決め、決定を合意ログに載せる。
ノード・装置が止まると仕事は他へ移るか縮退し、戻れば元に戻る。単一ノードでも同じ仕組みで動く
（[ADR-0013](../../dev/adr/0013-grid.md)、[9. クラスタとグリッド](09-cluster-grid.md)）。

### 中継（relay）

クラスタで、stream を受けていないノードに視聴者が来たとき、所有者のノードから符号化済みの AU をノード間の
1 本の多重化した接続で運び、手元の「写し」から配る（[ADR-0014](../../dev/adr/0014-relay.md)）。
stream ごとに上流は 1 本で、同じノードの視聴者が何人いても運ぶのは 1 回。

### リアルタイムの部屋

WebRTC の SFU（サイマルキャスト・スポットライト・転送フィルター・DataChannel・E2EE の素通し）。
シグナリングは WebSocket（`GET /signaling`）、参加の資格は参加トークン。部屋はグリッドの仕事として 1 つのノードに
置かれる（[ADR-0015](../../dev/adr/0015-realtime-rooms.md)、[10. リアルタイムの部屋](10-rooms.md)）。

### メディアパイプライン

部屋の参加者を stream に書き出し、合成・録画・RTMP などへの送り出し・文字起こしにつなぐ
（[ADR-0016](../../dev/adr/0016-media-pipelines.md)、[11. メディアパイプライン](11-media-pipelines.md)）。

## 1.3 データの流れの約束

- payload は受信時に一度だけ組み立て、以後は参照（`Bytes`）を配る。形を変える所（TS・fMP4 への多重化、暗号化）
  だけで新しいバイト列を作る。
- ノードの中の部品はメモリのチャネルでつなぐ。ループバックの TCP や外部のブローカーを通さない。
- 遅い購読者は取りこぼしを検出して次のキーフレームから再開する（他を待たせない）。新しい購読者には直近の GOP を先に渡す。

## 1.4 機能の一覧

| 分野 | できること | ページ |
|---|---|---|
| 受信 | RTMP/RTMPS（Enhanced RTMP: AV1・VP9・H.265）、SRT（listener・caller、AES）、WHIP | [4. 取り込みと再生](04-ingest-playback.md) |
| 再生 | HLS（TS・fMP4・LL-HLS）、RTMP play、WHEP、RTSP play、署名付きの再生 URL | [4. 取り込みと再生](04-ingest-playback.md) |
| 録画 | 録画と配信の分岐、VOD、MP4 の書き出し、予約、保持期間、法的保全、障害からの回収 | [5. 録画](05-recording.md) |
| 入力・出力 | RTMP・RTSP・SRT・HLS の pull、キャプチャ機器、RTMP・SRT の push、PaaS 連携、秘密値 | [6. 入力と出力](06-inputs-outputs.md) |
| 加工 | パイプライン（復号・合成・板書補正・音声ミキサー・符号化）、制作の操作、静止画・サムネイル | [7. パイプライン](07-pipelines.md) |
| 字幕 | 音声認識・翻訳の外部サービスにつないだ WebVTT の HLS 字幕 | [8. 字幕と翻訳](08-captions.md) |
| 分散 | Raft の制御面、mTLS、所有権と fencing、グリッドの配置と縮退、ノード間の中継 | [9. クラスタとグリッド](09-cluster-grid.md) |
| 部屋 | SFU、参加トークン、ブラウザー用 SDK | [10. リアルタイムの部屋](10-rooms.md) |
| メディアパイプライン | 部屋の書き出し・合成・録画・送り出し・文字起こし | [11. メディアパイプライン](11-media-pipelines.md) |
| 認証・保護 | API トークンと権限、JWT、署名付き URL、秘密値の封印、外へ出る接続の制限 | [12. セキュリティ](12-security.md) |
| 運用 | ヘルスチェック、ログ・監査ログ、SSE、回収、バックアップ、排出、終了 | [13. 運用](13-operations.md) |
| コーデック | ハードウェアとロイヤリティフリーのコーデック、特許の方針 | [14. コーデックと特許](14-codecs-and-patents.md) |
| API | REST（OpenAPI）、SSE、TypeScript・Rust のクライアント | [15. 管理 API と SDK](15-api-and-sdks.md) |

管理画面（`apps/console`）は `--console-dir` を指定すると `/console/` で配る。ブラウザーからの送出（WHIP）の
ページ（`apps/sender-web`）は `--sender-dir` で `/sender/`。

## 1.5 状態（何を確かめたか）

版は v0.0.0（実験的）。進み具合は [docs/dev/ROADMAP.md](../../dev/ROADMAP.md)、標準ごとの試験の状態は
[適合台帳](../../dev/conformance/ledger.md)、出荷の条件は [docs/dev/RELEASE.md](../../dev/RELEASE.md)。

**独立した実装と相互接続を確かめたもの**（`tools/interop/*.sh`）:

- RTMP（FFmpeg 7.0.2）、MPEG-TS・fMP4・HLS（FFmpeg）、HTTP/1.1（curl）
- SRT（libsrt 1.4.4 / 1.5.4）、RTP・SDP・RTSP（FFmpeg・mediamtx）、STUN・TURN（coturn）
- DTLS（OpenSSL 3.5）、SRTP（FFmpeg。AEAD-GCM は試験ベクターのみ）
- ロイヤリティフリーのコーデック（FFmpeg の復号器とビット一致）、JPEG・PNG（FFmpeg）
- WebRTC（WHIP・WHEP・部屋・メディアパイプライン・ブラウザー SDK）は Chrome で確認
- QUIC v1・HTTP/3・WebTransport・MoQ（aioquic・OpenSSL・Chrome・moq-dev と 43/43）。ノードの `--quic` は、HTTP/3 を
  aioquic・Chrome と、MoQ の購読を moq-dev の `moq export`・Chrome の WebTransport と確認しています（34/34）。MoQ の送出は SENCA 自身の client だけで確認しています

**自前の試験だけのもの**: Enhanced RTMP、LL-HLS（Apple の検証器・Safari は未実施）、制御 API、JWT、ICE、
Raft（決定的なシミュレーション）、codec worker の障害の隔離、外へ出る接続の制限、SBOM とライセンスの検査。

**確かめていないもの**:

- GPU のハードウェアコーデック（実機なし。装置が無い環境での検出だけ）、キャプチャ機器の実機
- PaaS 連携は模擬の API でだけ確認
- Firefox・Safari、TURN を通る経路
- 暗号・認可・分散の所有権の独立レビュー（RELEASE.md の G8）、24 時間の fuzz（60 秒の smoke のみ）、
  72 時間の連続試験、基準環境での負荷試験
- Android・Apple の送り手アプリはこの環境でビルドしていない

**未実装・制限**（ROADMAP・ADR の記録より）:

- ノードをまたぐ部屋のカスケード、録画の共有ストレージ（録画はノードの手元のディスク）
- 読みの線形化（ReadIndex）。読みは各ノードの手元の状態から
- 部屋の認証 Webhook・イベントの Webhook はノードに組み込まれていない（出来事は SSE だけ。[10. リアルタイムの部屋](10-rooms.md)）
- AAC は符号化・復号しない（パススルーだけ）。AAC の音声は WebRTC へ出せない
