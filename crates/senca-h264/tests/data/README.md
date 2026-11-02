# H.264 の試験データ

`tests/interop.rs` が読む。FFmpeg 7.0.2（静的ビルド、libx264）で作った。試験の実行時には FFmpeg を呼ばない。

## ビットストリーム（Annex B）

```sh
F=~/tools/ffmpeg
$F -f lavfi -i testsrc2=size=320x240:rate=30 -t 0.5 -c:v libx264 -preset fast -profile:v high -crf 40 -bf 2 -g 8 -keyint_min 8 -sc_threshold 0 -f h264 h264_320x240_high.h264
$F -f lavfi -i testsrc2=size=1918x1078:rate=25 -frames:v 1 -c:v libx264 -preset ultrafast -crf 51 -f h264 h264_1918x1078.h264
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv444p -c:v libx264 -preset ultrafast -crf 40 -f h264 h264_444.h264
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv420p10le -c:v libx264 -preset ultrafast -crf 40 -f h264 h264_10bit.h264
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 3 -c:v libx264 -profile:v baseline -preset ultrafast -crf 40 -f h264 h264_baseline.h264
$F -f lavfi -i testsrc2=size=718x476:rate=25 -frames:v 2 -c:v libx264 -preset ultrafast -crf 45 -x264-params interlaced=1:tff=1 -f h264 h264_interlaced.h264
```

## avcC（`*.avcC`）

FFmpeg の mp4 muxer が書いた AVCDecoderConfigurationRecord。

```sh
$F -i X.h264 -c copy -tag:v avc1 X.mp4
~/tools/ffprobe -v error -show_streams -show_data X.mp4   # extradata の 16 進をバイト列に戻して保存
```

## 期待値（ffprobe）

| ファイル | profile | level | 幅×高さ | 備考 |
|---|---|---|---|---|
| h264_320x240_high | High (100) | 13 | 320×240 | has_b_frames=2、15 packet、K は 0 と 8 |
| h264_1918x1078 | Constrained Baseline (66, 0xC0) | 40 | 1918×1078 | クロップあり |
| h264_444 | High 4:4:4 Predictive (244) | 11 | 176×144 | yuv444p |
| h264_10bit | High 10 (110) | 11 | 176×144 | yuv420p10le |
| h264_baseline | Constrained Baseline | 11 | 176×144 | 3 packet |
| h264_interlaced | Main (77, 0x40) | 30 | 718×476 | field_order=tt（MBAFF） |

codecs 文字列は FFmpeg の HLS muxer（`-f hls -hls_segment_type fmp4 -master_pl_name`）の CODECS と照合した。
