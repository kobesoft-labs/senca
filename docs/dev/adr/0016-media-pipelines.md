# ADR-0016 メディアパイプライン（AWS Chime SDK Media Pipelines 相当）

- 状態: 採用（2026-11-03、利用者の指示）
- 関係: ADR-0015、REQ-005、REQ-006、ADR-0011

| AWS（Chime SDK Media Pipelines） | SENCA |
|---|---|
| Media capture pipeline（会議を S3 へ録る） | 部屋の録画: 参加者ごと（音声・映像）と合成。置き場所はローカル・S3 互換 |
| Media concatenation pipeline | 録画の断片を 1 本の MP4・HLS にまとめる |
| Media live connector pipeline（会議を RTMP へ） | 部屋の合成を RTMP・SRT・WHIP・PaaS へ出す |
| Media insights pipeline（文字起こし・分析） | 参加者ごとの文字起こし・翻訳（ADR-0011）、話者の区間、音量 |
| Media stream pipeline（Kinesis Video Streams へ） | 部屋の参加者を SENCA の stream として書き出す（以後の HLS・録画・出力に乗る） |

- すべてグリッドの仕事（ADR-0013）として配置し、止まったノードの仕事は振り直す。
- 合成は codec worker のパイプライン（senca-pipeline）を使う。参加者を `source` にする。

## 実装の状況（2026-11-03）

- 書き出し・合成（grid・speaker。speaker は今は最初の参加者を大きくする）・録画・MP4 の書き出し・RTMP/SRT/PaaS への送り出し・字幕。
- 録画は stream の録画と同じ仕組みなので、ノードに保存先（S3 互換、ADR-0005）を設定すればそこへも上がる（部屋ごと・パイプラインごとに保存先は選べない）。送り出しの WHIP はまだ。
- `export_participants: false` でも合成のために参加者の stream は作る（録画・字幕を付けないだけ）。
