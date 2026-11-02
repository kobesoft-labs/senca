#!/usr/bin/env bash
# senca-pipeline（合成・板書補正・ミキサー・符号化）と codec worker のセッションの相互運用試験。
#
# 1. PinP の制作: FFmpeg（libvpx-vp9・libaom-av1・libopus）で作った 2 本の MP4（映像＋音声）を senca-fmp4 で読み、
#    復号 → PinP の合成・音声のミキシング → VP9＋Opus に符号化 → MP4。
#    FFmpeg が誤りなく復号できること、フレーム数・長さ、FFmpeg 自身の overlay で作った合成との PSNR、
#    2 つの音（440 Hz・1000 Hz）が入力と同じ大きさで混ざっていること、映像と音声の始まりが揃っていること。
# 2. 板書: FFmpeg の perspective で斜めにしたホワイトボード（字が途中で増える）を、既知の 4 隅で台形補正 →
#    元の平らな板との PSNR（FFmpeg の順変換と senca-board の逆変換の突き合わせ）、変化の静止画（JPEG）が出て
#    FFmpeg が読めること。
# 3. 単体・結合の試験: senca-pipeline の実行の試験（PSNR・フェード・スレート・メーター・世代）と、
#    本物の worker のプロセスでのセッション（実時間・拒否・テナントの上限・異常終了からの作り直し・背圧）。
#
# ライブラリは SENCA_CODEC_LIB_DIR（既定は ~/tools/codeclibs/usr/lib/x86_64-linux-gnu）から読み込む。
# FFmpeg は試験相手としてだけ使う。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
FFMPEG="${FFMPEG:-$HOME/tools/ffmpeg}"
FFPROBE="${FFPROBE:-$HOME/tools/ffprobe}"
PYTHON="${PYTHON:-python3}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export SENCA_CODEC_LIB_DIR="${SENCA_CODEC_LIB_DIR:-$HOME/tools/codeclibs/usr/lib/x86_64-linux-gnu}"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

ff() { "$FFMPEG" -nostdin -hide_banner -v error -y "$@"; }
probe() { "$FFPROBE" -v error "$@"; }
near() { awk -v a="$1" -v b="$2" -v t="$3" 'BEGIN { d = a - b; if (d < 0) d = -d; exit !(d <= t) }'; }

# 音の大きさ（Goertzel、左チャネル、s16le ステレオ）。
cat >"$WORK/tone.py" <<'EOF'
import sys, struct, math
# tone.py <pcm.s16le> <freq> <start_s> <len_s>
data = open(sys.argv[1], 'rb').read()
f = float(sys.argv[2]); start = int(float(sys.argv[3]) * 48000); n = int(float(sys.argv[4]) * 48000)
x = struct.unpack('<%dh' % (len(data) // 2), data)[0::2][start:start + n]
k = 2 * math.pi * f / 48000
re = sum(v * math.cos(k * i) for i, v in enumerate(x)); im = sum(v * math.sin(k * i) for i, v in enumerate(x))
print("%.4f" % (2 * math.hypot(re, im) / len(x) / 32768))
EOF

echo "== build"
(cd "$ROOT" && "$CARGO" build -q -p senca-pipeline --example pipeline_tool && "$CARGO" build -q -p senca-codec-worker)
TOOL="$CARGO_TARGET_DIR/debug/examples/pipeline_tool"

echo "== inputs (FFmpeg)"
W=320 H=240 FPS=30 DUR=3
ff -f lavfi -i "testsrc2=size=${W}x${H}:rate=$FPS:duration=$DUR" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$DUR" \
    -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 1500k -g 30 -pix_fmt yuv420p -c:a libopus -b:a 96k -ac 2 "$WORK/a.mp4"
ff -f lavfi -i "smptebars=size=${W}x${H}:rate=$FPS:duration=$DUR" -f lavfi -i "sine=frequency=1000:sample_rate=48000:duration=$DUR" \
    -c:v libaom-av1 -cpu-used 8 -b:v 1500k -g 30 -pix_fmt yuv420p -c:a libopus -b:a 96k -ac 2 "$WORK/b.mp4"
pass "ffmpeg made VP9+Opus and AV1+Opus inputs"

echo "== 1. PinP production (compose + audio mix)"
cat >"$WORK/pinp.json" <<EOF
{"version": 1,
 "options": {"clock": "input", "overflow": "block", "block_timeout_ms": 60000},
 "nodes": [
  {"type": "source", "id": "cam_a", "stream": "teacher", "video": {"codec": "vp9", "width": $W, "height": $H, "fps": $FPS}, "audio": {"codec": "opus"}},
  {"type": "source", "id": "cam_b", "stream": "board", "video": {"codec": "av1", "width": $W, "height": $H, "fps": $FPS}, "audio": {"codec": "opus"}},
  {"type": "decode", "id": "va", "input": "cam_a.video"},
  {"type": "decode", "id": "vb", "input": "cam_b.video"},
  {"type": "decode", "id": "aa", "input": "cam_a.audio"},
  {"type": "decode", "id": "ab", "input": "cam_b.audio"},
  {"type": "compose", "id": "prod", "inputs": {"main": "va", "inset": "vb"},
   "canvas": {"width": $W, "height": $H, "fps_num": $FPS},
   "scenes": {"pinp": {"layers": [
      {"source": "main", "x": 0, "y": 0, "width": $W, "height": $H, "fit": "stretch"},
      {"source": "inset", "x": 200, "y": 150, "width": 112, "height": 84, "fit": "stretch", "z": 1}]}},
   "program": "pinp"},
  {"type": "audio_mix", "id": "mix", "inputs": {"a": {"from": "aa"}, "b": {"from": "ab"}}},
  {"type": "encode", "id": "venc", "input": "prod", "codec": "vp9", "bitrate_kbps": 3000},
  {"type": "encode", "id": "aenc", "input": "mix", "codec": "opus", "bitrate_kbps": 128},
  {"type": "sink", "id": "out", "inputs": ["venc", "aenc"], "stream": "program"}
 ]}
EOF
if summary="$("$TOOL" "$WORK/pinp.json" "$WORK/out.mp4" cam_a="$WORK/a.mp4" cam_b="$WORK/b.mp4" 2>"$WORK/log")"; then
    pass "pipeline ran ($summary)"
else
    fail "pipeline run: $(tail -5 "$WORK/log")"
fi
if [[ -f "$WORK/out.mp4" ]] && ff -xerror -i "$WORK/out.mp4" -f null - 2>"$WORK/fferr" && [[ ! -s "$WORK/fferr" ]]; then
    pass "ffmpeg decodes the output without errors"
else
    fail "ffmpeg decode of the output: $(head -3 "$WORK/fferr" 2>/dev/null)"
fi
n="$(probe -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$WORK/out.mp4" || echo 0)"
if [[ "$n" == "$((FPS * DUR))" ]]; then pass "video frames: $n"; else fail "video frames: $n (expected $((FPS * DUR)))"; fi
vd="$(probe -select_streams v:0 -show_entries stream=duration -of csv=p=0 "$WORK/out.mp4" || echo 0)"
ad="$(probe -select_streams a:0 -show_entries stream=duration -of csv=p=0 "$WORK/out.mp4" || echo 0)"
if near "$vd" "$DUR" 0.05 && near "$ad" "$DUR" 0.1; then pass "durations video $vd s, audio $ad s"; else fail "durations video $vd s, audio $ad s"; fi
vs="$(probe -select_streams v:0 -show_entries stream=start_time -of csv=p=0 "$WORK/out.mp4" || echo 9)"
as="$(probe -select_streams a:0 -show_entries stream=start_time -of csv=p=0 "$WORK/out.mp4" || echo 9)"
if near "$vs" "$as" 0.04; then pass "A/V start aligned (video $vs s, audio $as s)"; else fail "A/V start video $vs s, audio $as s"; fi
# FFmpeg 自身の合成（overlay）と比べる
ff -i "$WORK/a.mp4" -i "$WORK/b.mp4" -filter_complex "[1:v]scale=112:84:flags=bicubic[s];[0:v][s]overlay=200:150" \
    -frames:v $((FPS * DUR)) -f rawvideo -pix_fmt yuv420p "$WORK/ref.yuv"
ff -i "$WORK/out.mp4" -f rawvideo -pix_fmt yuv420p "$WORK/ours.yuv"
psnr="$("$FFMPEG" -nostdin -hide_banner -v info -f rawvideo -pix_fmt yuv420p -s ${W}x${H} -r $FPS -i "$WORK/ours.yuv" \
    -f rawvideo -pix_fmt yuv420p -s ${W}x${H} -r $FPS -i "$WORK/ref.yuv" -lavfi psnr -f null - 2>&1 |
    sed -n 's/.*PSNR.* average:\([0-9.inf]*\).*/\1/p' | tail -1)"
if [[ -n "$psnr" ]] && awk -v p="$psnr" 'BEGIN { exit !(p == "inf" || p + 0 >= 27) }'; then
    pass "composition matches ffmpeg overlay (PSNR $psnr dB)"
else
    fail "composition vs ffmpeg overlay PSNR '$psnr'"
fi
ff -i "$WORK/out.mp4" -map 0:a -f s16le -ar 48000 -ac 2 "$WORK/mix.s16"
ff -i "$WORK/a.mp4" -map 0:a -f s16le -ar 48000 -ac 2 "$WORK/in_a.s16"
ff -i "$WORK/b.mp4" -map 0:a -f s16le -ar 48000 -ac 2 "$WORK/in_b.s16"
a440="$("$PYTHON" -I "$WORK/tone.py" "$WORK/mix.s16" 440 0.5 2)"
a1k="$("$PYTHON" -I "$WORK/tone.py" "$WORK/mix.s16" 1000 0.5 2)"
r440="$("$PYTHON" -I "$WORK/tone.py" "$WORK/in_a.s16" 440 0.5 2)"
r1k="$("$PYTHON" -I "$WORK/tone.py" "$WORK/in_b.s16" 1000 0.5 2)"
# ミキサーは 0 dB で足す: それぞれの音は入力（FFmpeg で復号した大きさ）と同じ大きさで残る。
if near "$a440" "$r440" "$(awk -v r="$r440" 'BEGIN { print r * 0.1 }')" && near "$a1k" "$r1k" "$(awk -v r="$r1k" 'BEGIN { print r * 0.1 }')"; then
    pass "audio mix keeps both tones: 440 Hz $a440 (input $r440), 1000 Hz $a1k (input $r1k)"
else
    fail "audio mix: 440 Hz $a440 (input $r440), 1000 Hz $a1k (input $r1k)"
fi

echo "== 2. whiteboard keystone + change snapshots"
BW=640 BH=360
# 平らな板（字が 1 秒・2 秒で増える）
BOARD="color=c=0xD8D8D8:s=${BW}x${BH}:r=$FPS:d=$DUR"
LETTERS=""
for i in 0 1 2 3 4 5 6 7 8; do
    LETTERS+=",drawbox=x=$((100 + i * 32)):y=100:w=22:h=26:color=black:t=fill:enable='gte(t,1)'"
    LETTERS+=",drawbox=x=$((150 + i * 36)):y=200:w=24:h=22:color=0x202060:t=fill:enable='gte(t,2)'"
done
ff -f lavfi -i "$BOARD$LETTERS" -pix_fmt yuv420p -f rawvideo "$WORK/flat.yuv"
# 斜めから撮った板（周りは暗い壁）: perspective で 4 隅を (120,60) (540,90) (90,300) (560,320) へ
ff -f rawvideo -pix_fmt yuv420p -s ${BW}x${BH} -r $FPS -i "$WORK/flat.yuv" \
    -vf "pad=iw:ih:0:0,perspective=120:60:540:90:90:300:560:320:sense=destination:eval=init" \
    -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 3000k -g 30 "$WORK/skewed.mp4"
cat >"$WORK/board.json" <<EOF
{"version": 1,
 "options": {"clock": "input", "overflow": "block", "block_timeout_ms": 60000},
 "nodes": [
  {"type": "source", "id": "cam", "stream": "board-raw", "video": {"codec": "vp9", "width": $BW, "height": $BH, "fps": $FPS}},
  {"type": "decode", "id": "dec", "input": "cam.video"},
  {"type": "board", "id": "fix", "input": "dec", "params": {"mode": "whiteboard",
     "corners": [[120, 60], [540, 90], [560, 320], [90, 300]], "output_width": $BW, "output_height": $BH, "enhance": false}},
  {"type": "snapshot_on_change", "id": "snap", "input": "fix", "params": {"analysis_fps": 10, "stable_frames": 3}},
  {"type": "encode", "id": "enc", "input": "fix", "codec": "vp9", "bitrate_kbps": 3000},
  {"type": "sink", "id": "out", "inputs": ["enc"], "stream": "board-clean"}
 ]}
EOF
mkdir -p "$WORK/snaps"
if summary="$("$TOOL" "$WORK/board.json" "$WORK/board.mp4" cam="$WORK/skewed.mp4" --snapshots "$WORK/snaps" 2>"$WORK/log")"; then
    pass "board pipeline ran ($summary)"
else
    fail "board pipeline: $(tail -5 "$WORK/log")"
fi
ff -i "$WORK/board.mp4" -f rawvideo -pix_fmt yuv420p "$WORK/board.yuv"
# 端の数画素は標本化の差が大きいので、内側を比べる
psnr="$("$FFMPEG" -nostdin -hide_banner -v info -f rawvideo -pix_fmt yuv420p -s ${BW}x${BH} -r $FPS -i "$WORK/board.yuv" \
    -f rawvideo -pix_fmt yuv420p -s ${BW}x${BH} -r $FPS -i "$WORK/flat.yuv" \
    -lavfi "[0:v]crop=600:320:20:20[a];[1:v]crop=600:320:20:20[b];[a][b]psnr" -f null - 2>&1 |
    sed -n 's/.*PSNR.* average:\([0-9.inf]*\).*/\1/p' | tail -1)"
if [[ -n "$psnr" ]] && awk -v p="$psnr" 'BEGIN { exit !(p == "inf" || p + 0 >= 24) }'; then
    pass "keystone-corrected board matches the flat board (PSNR $psnr dB)"
else
    fail "keystone-corrected board vs flat board PSNR '$psnr'"
fi
nsnap="$(find "$WORK/snaps" -name '*.jpg' | wc -l)"
if [[ "$nsnap" -ge 2 ]]; then pass "change snapshots: $nsnap"; else fail "change snapshots: $nsnap (expected >= 2)"; fi
if [[ "$nsnap" -ge 1 ]]; then
    dims="$(probe -show_entries stream=width,height -of csv=p=0 "$WORK/snaps/snap0.jpg" || true)"
    if [[ "$dims" == "$BW,$BH" ]]; then pass "snapshot JPEG decodes in ffmpeg ($dims)"; else fail "snapshot JPEG: '$dims'"; fi
fi

echo "== 3. runtime and worker-session tests"
if (cd "$ROOT" && SENCA_CODEC_REQUIRE_SW=1 "$CARGO" test -q -p senca-pipeline >"$WORK/log" 2>&1); then
    pass "senca-pipeline tests ($(grep -o '[0-9]* passed' "$WORK/log" | awk '{s += $1} END {print s}') passed: validation, generations, PinP/fade/slate, board, audio meters)"
else
    tail -30 "$WORK/log"
    fail "senca-pipeline tests"
fi
if (cd "$ROOT" && SENCA_CODEC_REQUIRE_SW=1 "$CARGO" test -q -p senca-codec-worker --test session >"$WORK/log" 2>&1); then
    pass "worker session tests through the real process incl. crash recovery and backpressure ($(grep -o '[0-9]* passed' "$WORK/log" | tail -1))"
else
    tail -30 "$WORK/log"
    fail "worker session tests"
fi

echo
echo "passed: $PASS, failed: $FAIL"
[[ $FAIL -eq 0 ]]
