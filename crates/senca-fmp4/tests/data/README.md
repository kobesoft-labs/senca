# 試験用の入力

FFmpeg 7.0.2（静的ビルド）で作った小さな fMP4（別実装が書いたものを読めることの確認用）。

```sh
# AV1 + Opus（moov にも最初のフラグメント分のサンプル表がある形）
ffmpeg -f lavfi -i testsrc2=size=64x48:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.8 \
  -c:v libaom-av1 -cpu-used 8 -g 10 -pix_fmt yuv420p -c:a libopus -b:a 24k \
  -movflags frag_keyframe+default_base_moof av1_opus_small.mp4

# H.264（B フレームあり）+ AAC、空の moov とフラグメント
ffmpeg -f lavfi -i testsrc2=size=64x48:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.8 \
  -c:v libx264 -bf 2 -g 10 -pix_fmt yuv420p -c:a aac -b:a 32k -ac 1 \
  -movflags frag_keyframe+empty_moov h264_aac_frag_small.mp4
```
