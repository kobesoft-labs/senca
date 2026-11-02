#!/usr/bin/env bash
# senca-hls の相互運用試験。
#
# 1. FFmpeg の hls muxer が作った playlist（TS・fMP4・single_file の byte range・multivariant・
#    live の窓・dash muxer の LL-HLS 向け出力）を senca-hls で読み、書き出し → 読み直しで同じになること、
#    書き出したものを FFmpeg で再生して元の playlist と同じフレームが出ることを確かめる。
# 2. FFmpeg が作った TS を senca-hls の Segmenter で切り（examples/segment_ts.rs）、出来た playlist を
#    FFmpeg で再生して元と同じフレームが出ること、GOP の途中の欠落で依存フレームを出さないこと
#    （FFmpeg が復号エラーを出さないこと）、part のファイルが segment と一致することを確かめる。
#
# 注: 静的ビルドの FFmpeg 7.0.2 は FFmpeg 自身が書いた TS（SDT を含む）を直接読むと落ちることがあるため、
#     比較の基準は同じ符号化データを持つ Matroska から取る。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
FFMPEG="${FFMPEG:-$HOME/tools/ffmpeg}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

ff() { "$FFMPEG" -nostdin -hide_banner -v error -y "$@"; }
# ストリームごとの復号フレームの md5 列（時刻は比べない）
hashes() { "$FFMPEG" -nostdin -v error -i "$1" -f framemd5 - | grep -v '^#' | awk -F', *' '{print $1, $6}' | sort -s -k1,1n; }
same_frames() { # same_frames <名前> <基準> <対象>
    local a b
    a="$(hashes "$2")" || { fail "$1 (decode $2)"; return; }
    b="$(hashes "$3")" || { fail "$1 (decode $3)"; return; }
    if [[ -n "$a" && "$a" == "$b" ]]; then
        pass "$1 ($(wc -l <<<"$a") frames identical)"
    else
        fail "$1 (frames differ: $(wc -l <<<"$a") vs $(wc -l <<<"$b"))"
    fi
}

"$CARGO" build -q --manifest-path "$ROOT/Cargo.toml" -p senca-hls --examples
EX="$CARGO_TARGET_DIR/debug/examples"
cd "$WORK"

# --- 素材 ---
ff -f lavfi -i testsrc2=size=320x180:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 12 \
    -c:v libx264 -g 30 -keyint_min 30 -sc_threshold 0 -bf 2 -c:a aac src.mkv
ff -i src.mkv -c copy -f mpegts in.ts
ff -f lavfi -i testsrc2=size=320x180:rate=30 -t 9 -c:v libx264 -g 45 -keyint_min 45 -sc_threshold 0 -bf 0 gop45.mkv
ff -i gop45.mkv -c copy -f mpegts gop45.ts
ff -f lavfi -i sine=frequency=440:sample_rate=48000 -t 7 -c:a aac audio.mkv
ff -i audio.mkv -c copy -f mpegts audio.ts

# --- 1. FFmpeg の playlist を読む ---
mkdir -p h/ts h/fmp4 h/single h/mv h/live h/dash
ff -i src.mkv -c copy -f hls -hls_time 2 -hls_playlist_type vod -hls_flags program_date_time+independent_segments h/ts/index.m3u8
ff -i src.mkv -c copy -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_type fmp4 h/fmp4/index.m3u8
ff -i src.mkv -c copy -f hls -hls_time 2 -hls_flags single_file h/single/index.m3u8
ff -i src.mkv -map 0:v -map 0:v -map 0:a -c:v libx264 -g 30 -b:v:0 400k -s:v:0 320x180 -b:v:1 200k -s:v:1 160x90 \
    -c:a aac -b:a 64k -f hls -hls_time 2 -hls_playlist_type vod -master_pl_name master.m3u8 \
    -var_stream_map "v:0,agroup:aac v:1,agroup:aac a:0,agroup:aac,default:yes,language:ENG,name:English" h/mv/vs%v.m3u8
ff -i src.mkv -c copy -f hls -hls_time 2 -hls_list_size 3 -hls_flags delete_segments+omit_endlist h/live/index.m3u8
ff -i src.mkv -c copy -strict experimental -f dash -hls_playlist 1 -lhls 1 -streaming 1 -seg_duration 2 \
    -frag_duration 0.5 -frag_type duration h/dash/manifest.mpd

for f in h/*/*.m3u8; do
    if out="$("$EX/playlist_check" "$f" --rewrite)"; then pass "parse+roundtrip ffmpeg ${f#h/}: ${out#OK *: }"; else fail "$out"; fi
done
grep -q '^#EXT-X-STREAM-INF:' h/mv/master.m3u8.senca.m3u8 && pass "multivariant has STREAM-INF" || fail "multivariant STREAM-INF"
# 静的ビルドの FFmpeg が SDT で落ちるのを避けるため、FFmpeg が書いた TS の SDT packet を null packet に置き換える
# （長さは変えないので byte range はそのまま）
cat >"$WORK/nosdt.py" <<'PY'
import sys
for path in sys.argv[1:]:
    d = bytearray(open(path, "rb").read())
    for i in range(0, len(d) - 187, 188):
        if d[i] == 0x47 and ((d[i + 1] & 0x1F) << 8 | d[i + 2]) == 0x11:
            d[i + 1] = (d[i + 1] & 0xE0) | 0x1F
            d[i + 2] = 0xFF
    open(path, "wb").write(bytes(d))
PY
python3 -I "$WORK/nosdt.py" h/*/*.ts
for f in h/ts/index.m3u8 h/fmp4/index.m3u8 h/single/index.m3u8 h/mv/master.m3u8 h/dash/media_0.m3u8; do
    same_frames "rewritten ${f#h/} plays like original" "$f" "$f.senca.m3u8"
done

# --- 2. Segmenter ---
"$EX/segment_ts" in.ts s_vod --part-ms 500 2>s_vod.log
same_frames "segmenter VOD (LL parts) vs source" src.mkv s_vod/index.m3u8
grep -q '^#EXT-X-PLAYLIST-TYPE:VOD' s_vod/index.m3u8 && grep -q '^#EXT-X-ENDLIST' s_vod/index.m3u8 \
    && pass "VOD playlist has PLAYLIST-TYPE:VOD and ENDLIST" || fail "VOD tags"
if out="$("$EX/playlist_check" s_vod/index.m3u8 s_vod/midway.m3u8)"; then pass "segmenter playlists re-parse identically"; else fail "$out"; fi

# part のファイル（それぞれ PAT/PMT 付き）を並べると segment と同じ中身になる
psi=376
ok=1
for seg in s_vod/seg[0-9].ts s_vod/seg[0-9][0-9].ts; do
    [[ -e "$seg" ]] || continue
    base="${seg%.ts}"
    parts=("$base".[0-9]*.ts)
    [[ -e "${parts[0]}" ]] || continue
    sum=0
    for p in "${parts[@]}"; do sum=$((sum + $(stat -c %s "$p") - psi)); done
    if [[ $((sum + psi)) != "$(stat -c %s "$seg")" ]]; then ok=0; echo "  mismatch: $seg"; fi
    if ! cmp -s <(for p in $(ls "$base".[0-9]*.ts | sort -t. -k2,2n); do tail -c +$((psi + 1)) "$p"; done) <(tail -c +$((psi + 1)) "$seg"); then
        ok=0
        echo "  content mismatch: $seg"
    fi
done
[[ $ok == 1 ]] && pass "parts concatenate to segments" || fail "parts vs segments"
first_part="$(ls s_vod/seg*.0.ts | head -1)"
if [[ -z "$(ff -i "$first_part" -f null - 2>&1)" ]]; then pass "independent part decodes standalone ($first_part)"; else fail "independent part decode"; fi

# GOP 1.5 秒・target 2 秒 → 3 秒の segment、TARGETDURATION は 3 に上がる
"$EX/segment_ts" gop45.ts s_gop 2>/dev/null
same_frames "segmenter GOP 1.5s vs source" gop45.mkv s_gop/index.m3u8
td="$(sed -n 's/^#EXT-X-TARGETDURATION://p' s_gop/index.m3u8)"
durs="$(sed -n 's/^#EXTINF:\([0-9.]*\),.*/\1/p' s_gop/index.m3u8 | sort -u | tr '\n' ' ')"
[[ "$td" == 3 ]] && pass "long GOP raises TARGETDURATION to 3 (EXTINF: $durs)" || fail "TARGETDURATION=$td"

# 音声だけ
"$EX/segment_ts" audio.ts s_audio 2>/dev/null
same_frames "segmenter audio-only vs source" audio.mkv s_audio/index.m3u8

# GOP の途中の欠落: 次の keyframe まで捨て、DISCONTINUITY を付ける。FFmpeg が復号エラーを出さないこと
"$EX/segment_ts" in.ts s_gap --kind live --window 4 --part-ms 300 --gap-at-sec 5.3 2>s_gap.log
errs="$(ff -i s_gap/index.m3u8 -f null - 2>&1 || echo "rc=$?")"
if [[ -z "$errs" ]]; then pass "gap mid-GOP: playback without decode errors ($(cat s_gap.log))"; else fail "gap playback: $(head -3 <<<"$errs")"; fi
grep -q '^#EXT-X-DISCONTINUITY$' s_gap/midway.m3u8 s_gap/index.m3u8 && pass "gap produces EXT-X-DISCONTINUITY" || fail "no DISCONTINUITY"
if out="$("$EX/playlist_check" s_gap/index.m3u8 s_gap/midway.m3u8 s_gap/midway_delta.m3u8)"; then pass "live LL-HLS playlists re-parse identically"; else fail "$out"; fi
grep -q '^#EXT-X-PRELOAD-HINT:TYPE=PART' s_gap/midway.m3u8 && grep -q 'CAN-BLOCK-RELOAD=YES' s_gap/midway.m3u8 \
    && pass "live playlist has PRELOAD-HINT and SERVER-CONTROL" || fail "LL-HLS tags"
# 窓の playlist（終了後は ENDLIST 付き）を再生したフレーム数 ≒ EXTINF の合計 × 30
frames="$(hashes s_gap/index.m3u8 | awk '$1==0' | wc -l)"
expect="$(sed -n 's/^#EXTINF:\([0-9.]*\),.*/\1/p' s_gap/index.m3u8 | awk '{s+=$1} END {printf "%d", s*30 + 0.5}')"
if (( frames >= expect - 1 && frames <= expect + 1 )); then pass "live window frames $frames ~ EXTINF sum $expect"; else fail "live window frames $frames vs $expect"; fi

echo "----"
echo "passed: $PASS, failed: $FAIL"
[[ "$FAIL" == 0 ]]
