# AV1 の試験データ

`tests/interop.rs` が読む。FFmpeg 7.0.2（静的ビルド、libaom-av1）で作った。試験の実行時には FFmpeg を呼ばない。

## ビットストリーム（Low Overhead Bitstream Format、Temporal Delimiter 付き）

```sh
F=~/tools/ffmpeg
$F -f lavfi -i testsrc2=size=320x240:rate=30 -t 0.3 -c:v libaom-av1 -cpu-used 8 -crf 50 -g 5 -f obu av1_320x240.obu
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv420p10le -c:v libaom-av1 -cpu-used 8 -crf 50 -f obu av1_10bit.obu
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv444p -c:v libaom-av1 -cpu-used 8 -crf 50 -f obu av1_444.obu
```

## av1C（`*.av1C`）

FFmpeg の mp4 muxer が書いた AV1CodecConfigurationRecord。

```sh
$F -i X.obu -c copy -tag:v av01 X.mp4
~/tools/ffprobe -v error -show_streams -show_data X.mp4   # extradata の 16 進をバイト列に戻して保存
```

## 期待値（ffprobe）

| ファイル | profile | 幅×高さ | pix_fmt | packet の K |
|---|---|---|---|---|
| av1_320x240 | Main (0) | 320×240 | yuv420p | 9 packet、K は 0 と 5 |
| av1_10bit | Main (0) | 176×144 | yuv420p10le | 2 packet、K は 0 |
| av1_444 | High (1) | 176×144 | yuv444p | 2 packet、K は 0 |
