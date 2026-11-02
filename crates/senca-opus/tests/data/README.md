# Opus の試験データ

`tests/interop.rs` が読む（Ogg の解析は試験の中の最小限の実装）。FFmpeg 7.0.2（静的ビルド、libopus）で作った。
試験の実行時には FFmpeg を呼ばない。

```sh
F=~/tools/ffmpeg
$F -f lavfi -i sine=frequency=440:sample_rate=48000 -ac 2 -t 0.3 -c:a libopus -b:a 32k -f ogg opus_stereo.opus
$F -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.3 -ac 1 -c:a libopus -frame_duration 60 -b:a 16k -f ogg opus_mono_60ms.opus
```

## 期待値（ffprobe -show_entries packet=duration,size）

| ファイル | チャンネル | pre_skip | packet の長さ | packet の大きさ |
|---|---|---|---|---|
| opus_stereo | 2 | 312 | 960（最後だけ末尾の切り詰めで 312） | 84 71 75 78 76 77 73 74 79 57 60 61 65 66 57 39 |
| opus_mono_60ms | 1 | 312 | 2880（最後だけ 312） | 179 171 163 121 122 68 |
