# ADR-0003 内部のメディア表現

- 状態: 採用（2026-11-02）

`senca-core::media::AccessUnit` を全経路で使う。payload は `Bytes`（参照カウント）で、受信時に一度だけ組み立てる。

| コーデック | payload | 設定（CodecConfig.record） |
|---|---|---|
| H.264 | 4 バイト長さ前置の NAL 列（AVCC） | AVCDecoderConfigurationRecord |
| H.265 | 4 バイト長さ前置の NAL 列（HVCC） | HEVCDecoderConfigurationRecord |
| AV1 | OBU 列（Temporal Delimiter を除く） | AV1CodecConfigurationRecord |
| VP8 / VP9 | フレーム | 空（VP9 は vpcC があれば） |
| AAC | raw_data_block（ADTS なし） | AudioSpecificConfig |
| Opus | Opus パケット | OpusHead |
| G.711 | サンプル列 | 空 |

時刻は AU ごとの `timebase` で持つ（入力の時間軸のまま。RTMP は 1/1000、TS・RTP 映像は 1/90000）。換算は `senca-core::time::Rescale`（i128、丸め規則あり、溢れたら None）。DTS がない入力に PTS=DTS を無条件に当てない（仕様 4.2）。

設定の変化は `CodecConfig.generation` を増やし、AU の `config_generation` で参照する。
