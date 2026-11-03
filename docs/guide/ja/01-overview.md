# 1. 概要

SENCA は、ライブ配信・録画・リアルタイム通信のためのメディアサーバーです（Rust 製、Apache-2.0 ライセンス）。
1 台の `senca-node` だけでも、複数のノードをまとめたクラスター（グリッド）としても動作します。

このページでは、SENCA を構成するプログラム、内部の構成、データの扱い方、機能の一覧、動作確認の状況を説明します。

- 関連するページ: [2. クイックスタート](02-quickstart.md)、[3. 設定](03-configuration.md)、[12. セキュリティ](12-security.md)、[13. 運用](13-operations.md)
- 設計の背景: [ADR-0001 全体構成](../../dev/adr/0001-architecture.md)

## 1.1 実行ファイル

SENCA は次の 3 つの実行ファイルで構成されます。

| 実行ファイル | 役割 |
|---|---|
| `senca-node` | 本体です。取り込み（gateway）・メディア処理・配信（origin）・制御（control）を 1 つのプロセスで行います |
| `senca-codec-worker` | コーデックを実行する別プロセスです。`senca-node` が複数起動してプールとして使い、パイプで通信します（大きなフレームは共有メモリで渡します）。サンドボックスの中で動作します |
| `senca-capture-worker` | キャプチャデバイス（V4L2・ALSA・DeckLink）を扱う別プロセスです。このファイルがない場合は、キャプチャデバイスが使えなくなるだけです |

`senca-node` は、自身と同じディレクトリにある `senca-codec-worker`・`senca-capture-worker` を探して起動します。
codec worker の場所は `--codec-worker-bin` で変更できます。

## 1.2 構成

全体の構成は次のとおりです。取り込んだストリームは、ストリームごとの hub に集まり、そこから配信・録画・加工へ分かれます。

```
            RTMP/RTMPS  SRT  WHIP  RTSP/RTMP/SRT/HLS pull  キャプチャ
                 │       │     │            │                 │
                 ▼       ▼     ▼            ▼                 ▼
          ┌──────────────────────────────────────────────────────────┐
          │ stream hub（ストリームごとに 1 タスク、上限付きのキュー）   │
          │   ├─ packager → LiveCache（メモリ）→ HTTP: HLS（TS/fMP4/LL-HLS）
          │   ├─ 購読者: RTMP play・WHEP・RTSP play・push 出力・パイプライン
          │   └─ recorder → segments/（変更しないファイル）＋ recordings/{id}/index.jsonl
          └──────────────────────────────────────────────────────────┘
                 │ エンコード・デコード・合成            ▲
                 ▼                                       │
          senca-codec-worker（別プロセス・サンドボックス）     │
                                                         │
  管理 API（REST・SSE）→ 制御状態（決定的な状態機械）──────┘
        単一ノード: control/log（追記・fsync）／クラスター: Raft（control/raft/）
        グリッド: ワークロード（入力・出力・パイプライン・部屋）をノードへ配置
        中継: エンコード済みの AU をノード間で転送
```

### 制御プレーン

- ストリーム・トークン・録画の情報・予約・入力・出力・秘密値・パイプライン・部屋などの設定は、「制御状態」として
  管理されます。制御状態は、コマンドを順に適用して更新する決定的な状態機械です。
- 単一ノードでは、適用したコマンドを 1 件ずつ `control/log` に追記して fsync してから、API の応答を返します。
  1000 件ごとに状態全体を `control/snapshot.json` に保存し、ログを空にします。
- クラスターでは、同じコマンドを独自実装の Raft で複製します。投票者は 1〜7 台です。どのノードに書き込んでも、
  リーダーへ転送されます。過半数のノードと通信できなくなった側では、書き込みができません（503）。読み取りは、
  各ノードが手元に持つ状態から返します。詳しくは [ADR-0009](../../dev/adr/0009-cluster.md) と [9. クラスターとグリッド](09-cluster-grid.md) を参照してください。

### stream hub・packager・recorder

- **hub**（[ADR-0006](../../dev/adr/0006-stream-hub.md)）: ストリームごとに 1 つのタスクとして動きます。hub は接続が切れても残り、
  配信者が猶予時間（`--reconnect-grace`、既定 30 秒）内に再接続すれば、同じストリームとして配信が続きます。
  入力のキューには上限があり、hub の処理が遅れると配信者からの読み込みを止めます（TCP の背圧）。
- **packager**: セグメントの分割と多重化を hub の中で 1 回だけ行い、結果をメモリ上のキャッシュ（`LiveCache`）に置きます。
  ライブの HLS はディスクを使いません。
- **recorder**（[ADR-0005](../../dev/adr/0005-recording.md)）: 録画しているストリームがある間だけ動作します。ライブ配信と同じ
  セグメントを `segments/` に変更しないファイルとして書き込み、録画ごとのインデックス（`recordings/{id}/index.jsonl`）に追記します。
  キューがあふれた場合は、録画側でセグメントを破棄して欠落（gap）として記録します。ライブ配信は止めません。
  起動時には、書き込み途中だった録画を `interrupted` として閉じます（[13. 運用](13-operations.md)）。

### codec worker のサンドボックス

H.264/H.265 のエンコード・デコードは GPU メーカーの SDK（NVENC/NVDEC・oneVPL・AMF・VA-API）で、AV1・VP8/VP9・Opus は
ロイヤリティフリーのライブラリ（dav1d・SVT-AV1・libvpx・libopus）で行います。これらのライブラリは実行時に `dlopen` で
読み込みますが、`senca-node` 自身には読み込みません。読み込むのは `senca-codec-worker` で、seccomp・Landlock・rlimit などで
権限を制限した別プロセスとして動作します（[12. セキュリティ](12-security.md)、[ADR-0004](../../dev/adr/0004-codecs-patents.md)、
[14. コーデックと特許](14-codecs-and-patents.md)）。

codec worker やライブラリがない場合でも、受信したストリームをそのまま配信・録画することはできます。
変換・静止画・サムネイルは `503 CAPACITY_UNAVAILABLE` になります。

### グリッド

グリッドは、ノードの資源（CPU・メモリ・帯域・GPU・キャプチャデバイス）を集計し、ワークロード（pull 入力・push 出力・
パイプライン・部屋・メディアパイプライン）をノードに配置する仕組みです。配置は Raft のリーダーが決め、その決定を
複製されるログに記録します。ノードやデバイスが停止すると、ワークロードは別のノードへ移るか、縮退して動作を続けます。
資源が復旧すると元に戻ります。単一ノードでも同じ仕組みで動作します（[ADR-0013](../../dev/adr/0013-grid.md)、
[9. クラスターとグリッド](09-cluster-grid.md)）。

### 中継（relay）

クラスターでは、ストリームを受信していないノードに視聴者が接続することがあります。この場合、そのノードは所有者のノードから
エンコード済みの AU を、ノード間の 1 本の多重化された接続で受け取り、手元のミラーから配信します（[ADR-0014](../../dev/adr/0014-relay.md)）。
上流からの転送はストリームごとに 1 本で、同じノードに視聴者が何人いても、転送は 1 回分だけです。

### リアルタイムの部屋

WebRTC の SFU です。サイマルキャスト・スポットライト・転送フィルター・DataChannel・E2EE のパススルーに対応しています。
シグナリングには WebSocket（`GET /signaling`）を使い、参加には参加トークンが必要です。部屋はグリッドのワークロードとして、
1 つのノードに配置されます（[ADR-0015](../../dev/adr/0015-realtime-rooms.md)、[10. リアルタイムの部屋](10-rooms.md)）。

### メディアパイプライン

部屋の参加者を SENCA のストリームとして書き出し、合成・録画・RTMP などへの送信・文字起こしにつなぎます
（[ADR-0016](../../dev/adr/0016-media-pipelines.md)、[11. メディアパイプライン](11-media-pipelines.md)）。

## 1.3 データの扱い方

SENCA は、メディアデータを次の方針で扱います。

- 受信したデータは一度だけ組み立て、その後はコピーせずに参照を共有します。新しいバイト列を作るのは、形式を変える処理
  （TS・fMP4 への多重化、暗号化）のときだけです。
- ノード内の各部分は、メモリ上のチャネルでつながっています。ループバックの TCP や外部のメッセージブローカーは経由しません。
- 処理の遅い購読者は、データを取りこぼしたことを検出すると、次のキーフレームから再開します。他の購読者を待たせることはありません。
  新しく接続した購読者には、直近の GOP を先に渡します。

## 1.4 機能の一覧

| 分野 | 主な機能 | ページ |
|---|---|---|
| 取り込み | RTMP/RTMPS（Enhanced RTMP: AV1・VP9・H.265）、SRT（listener・caller、AES 暗号化）、WHIP | [4. 取り込みと再生](04-ingest-playback.md) |
| 再生 | HLS（TS・fMP4・LL-HLS）、RTMP play、WHEP、RTSP play、署名付きの再生 URL | [4. 取り込みと再生](04-ingest-playback.md) |
| 録画 | ライブ配信と並行した録画、VOD、MP4 の書き出し、予約録画、保持期間、法的保全（legal hold）、障害後の復旧 | [5. 録画](05-recording.md) |
| 入力・出力 | RTMP・RTSP・SRT・HLS の pull、キャプチャデバイス、RTMP・SRT の push、PaaS 連携、秘密値 | [6. 入力と出力](06-inputs-outputs.md) |
| 加工 | パイプライン（デコード・合成・板書補正・音声ミキサー・エンコード）、配信制作の操作、静止画・サムネイル | [7. パイプライン](07-pipelines.md) |
| 字幕 | 外部の音声認識・翻訳サービスと連携した、WebVTT による HLS の字幕 | [8. 字幕と翻訳](08-captions.md) |
| クラスター | Raft による制御プレーン、mTLS、所有権と fencing、グリッドの配置と縮退、ノード間の中継 | [9. クラスターとグリッド](09-cluster-grid.md) |
| 部屋 | SFU、参加トークン、ブラウザー用 SDK | [10. リアルタイムの部屋](10-rooms.md) |
| メディアパイプライン | 部屋の書き出し・合成・録画・外部への送信・文字起こし | [11. メディアパイプライン](11-media-pipelines.md) |
| 認証・保護 | API トークンと権限、JWT、署名付き URL、秘密値の暗号化保存、外部への接続の制限 | [12. セキュリティ](12-security.md) |
| 運用 | ヘルスチェック、ログ・監査ログ、SSE、障害後の復旧、バックアップ、drain、停止 | [13. 運用](13-operations.md) |
| コーデック | ハードウェアコーデックとロイヤリティフリーのコーデック、特許への対応方針 | [14. コーデックと特許](14-codecs-and-patents.md) |
| API | REST（OpenAPI）、SSE、TypeScript・Rust のクライアント | [15. 管理 API と SDK](15-api-and-sdks.md) |

管理画面（`apps/console`）は、`--console-dir` を指定すると `/console/` で配信されます。ブラウザーから WHIP で配信するための
ページ（`apps/sender-web`）は、`--sender-dir` を指定すると `/sender/` で配信されます。

## 1.5 動作確認の状況

このガイドの対象は v0.0.1 です。開発の状況は [docs/dev/ROADMAP.md](../../dev/ROADMAP.md) に、標準規格ごとのテストの状況は
[適合台帳](../../dev/conformance/ledger.md) に、リリース前に行う確認は [docs/dev/RELEASE.md](../../dev/RELEASE.md) にまとめています。

**独立した他の実装との相互接続を確認しているもの**:

- RTMP（FFmpeg 7.0.2）、MPEG-TS・fMP4・HLS（FFmpeg）、HTTP/1.1（curl）
- SRT（libsrt 1.4.4 / 1.5.4）、RTP・SDP・RTSP（FFmpeg・mediamtx）、STUN・TURN（coturn）
- DTLS（OpenSSL 3.5）、SRTP（FFmpeg。AEAD-GCM はテストベクターでのみ確認）
- ロイヤリティフリーのコーデック（FFmpeg のデコーダーと出力がビット単位で一致）、JPEG・PNG（FFmpeg）
- WebRTC（WHIP・WHEP・部屋・メディアパイプライン・ブラウザー用 SDK）: Chrome
- QUIC v1・HTTP/3・WebTransport・MoQ: aioquic・OpenSSL・Chrome・moq-dev。ノードの `--quic` では、HTTP/3 を aioquic・Chrome で、
  MoQ の購読を moq-dev の `moq export`・Chrome の WebTransport で確認しています。MoQ での配信（送信）は、SENCA 自身のクライアントで確認しています

**SENCA 自身のテストで確認しているもの**: Enhanced RTMP、LL-HLS（Apple の検証ツール・Safari での確認は未実施）、管理 API、JWT、ICE、
Raft（決定的なシミュレーション）、codec worker の障害の隔離、外部への接続の制限、SBOM とライセンスの検査。

**現在確認していないもの**:

- GPU のハードウェアコーデック（実機での確認は未実施。デバイスがない環境での検出のみ確認）、キャプチャデバイスの実機
- PaaS 連携（モックの API でのみ確認）
- Firefox・Safari、TURN を経由するメディアの経路
- Android・Apple 向けの配信アプリのビルド

**現在対応していないこと**:

- 録画の共有ストレージ（録画は各ノードのローカルディスクに保存されます）
- 読み取りの線形化（ReadIndex）。読み取りは各ノードが手元に持つ状態から返します
- AAC のエンコード・デコード（パススルーのみ対応）。そのため、AAC の音声を WebRTC へ送ることはできません
