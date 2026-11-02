# H.265 の試験データ

`tests/interop.rs` が読む。FFmpeg 7.0.2（静的ビルド、libx265）で作った。試験の実行時には FFmpeg を呼ばない。

## ビットストリーム（Annex B）

```sh
F=~/tools/ffmpeg
$F -f lavfi -i testsrc2=size=320x240:rate=30 -t 0.3 -c:v libx265 -x265-params log-level=none -crf 40 -f hevc hevc_320x240.h265
$F -f lavfi -i testsrc2=size=1918x1078:rate=25 -frames:v 1 -c:v libx265 -x265-params log-level=none -preset ultrafast -crf 51 -f hevc hevc_1918x1078.h265
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv420p10le -c:v libx265 -x265-params log-level=none -crf 40 -f hevc hevc_main10.h265
$F -f lavfi -i testsrc2=size=176x144:rate=30 -frames:v 2 -pix_fmt yuv444p -c:v libx265 -x265-params log-level=none -crf 40 -f hevc hevc_444.h265
```

## hvcC（`*.hvcC`）

FFmpeg の mp4 muxer が書いた HEVCDecoderConfigurationRecord。

```sh
$F -i X.h265 -c copy -tag:v hvc1 X.mp4
~/tools/ffprobe -v error -show_streams -show_data X.mp4   # extradata の 16 進をバイト列に戻して保存
```

## 期待値（ffprobe）

| ファイル | profile | level | 幅×高さ | 備考 |
|---|---|---|---|---|
| hevc_320x240 | Main (1) | 60 | 320×240 | has_b_frames=2、9 packet、K は 0 だけ |
| hevc_1918x1078 | Main (1) | 120 | 1918×1078 | coded 1920×1088（conformance window） |
| hevc_main10 | Main 10 (2) | 60 | 176×144 | yuv420p10le |
| hevc_444 | Rext (4) | 60 | 176×144 | yuv444p |

codecs 文字列は ISO/IEC 14496-15 附属書 E に従って手で求めた値と照合した
（FFmpeg 7.0 の HLS muxer は互換フラグと制約バイトを規格どおりに書かないので照合に使っていない）。
