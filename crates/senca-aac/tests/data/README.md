# AAC の試験データ

`tests/interop.rs` が読む。FFmpeg 7.0.2（静的ビルド、内蔵の aac エンコーダー）で作った。試験の実行時には FFmpeg を呼ばない。
製品は AAC を符号化・復号しない（ADR-0004）。ここでは試験データを作るためだけに FFmpeg を使った。

## ADTS

```sh
F=~/tools/ffmpeg
$F -f lavfi -i sine=frequency=440:sample_rate=44100 -ac 1 -t 0.3 -c:a aac -b:a 32k -f adts aac_44k_mono.aac
$F -f lavfi -i sine=frequency=440:sample_rate=48000 -ac 2 -t 0.3 -c:a aac -b:a 64k -f adts aac_48k_stereo.aac
$F -f lavfi -i sine=frequency=440:sample_rate=48000 -ac 6 -t 0.2 -c:a aac -b:a 128k -f adts aac_48k_51.aac
```

## AudioSpecificConfig（`*.asc`）

```sh
$F -i X.aac -c copy X.mp4
~/tools/ffprobe -v error -show_streams -show_data X.mp4   # extradata の 16 進をバイト列に戻して保存
```

## 期待値（ffprobe）

| ファイル | profile | 周波数 | チャンネル | packet の大きさ（ADTS ヘッダー込み） |
|---|---|---|---|---|
| aac_44k_mono | LC | 44100 | 1 | 158 168 68 77 84 76 85 114 89 95 101 114 100 79 |
| aac_48k_stereo | LC | 48000 | 2 | 137 231 125 128 136 140 149 142 166 170 164 175 152 161 175 14 |
| aac_48k_51 | LC | 48000 | 6（5.1） | 212 421 136 183 182 185 185 182 188 329 26 |

codecs 文字列 `mp4a.40.2` は FFmpeg の HLS muxer の CODECS と照合した。
