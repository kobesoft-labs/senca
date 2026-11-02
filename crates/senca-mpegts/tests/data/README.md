# 試験用の入力

FFmpeg 7.0.2（静的ビルド）で作った小さな TS。FFmpeg が出す SDT（PID 0x11）も含む（分離器が無視することの確認を兼ねる）。

```sh
ffmpeg -f lavfi -i testsrc2=size=64x48:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.8 \
  -c:v libx264 -bf 2 -g 10 -pix_fmt yuv420p -c:a aac -b:a 32k -ac 1 \
  -f mpegts -mpegts_flags +resend_headers h264_aac_small.ts

ffmpeg -f lavfi -i testsrc2=size=64x48:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.8 \
  -c:v libx265 -x265-params log-level=error -bf 2 -g 10 -pix_fmt yuv420p -c:a libopus -b:a 24k -ac 2 \
  -f mpegts h265_opus_small.ts
```
