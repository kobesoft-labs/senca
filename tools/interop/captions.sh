#!/usr/bin/env bash
# senca-caption と FFmpeg の突き合わせ（FFmpeg は試験の相手としてだけ使う）。
#
# 1. FFmpeg で作った AAC（m4a）・Opus（mp4）を senca-caption の AudioChunker で切り（復号しない）、
#    出来た .m4a・.aac・.ogg を FFmpeg で復号して、誤りが出ないこと・サンプル数がチャンクの長さと合うこと
#    （±1 サンプル）、ffprobe が形式を認識することを確かめる。
# 2. 字幕の .vtt（録画用の全体・HLS のセグメント）・字幕の media playlist・VOD の playlist・
#    EXT-X-MEDIA TYPE=SUBTITLES 付きの multivariant playlist を FFmpeg で読み、Cue が出ること、
#    multivariant に字幕のストリームが現れることを確かめる。
#
# 環境変数: FFMPEG・FFPROBE（既定 ~/tools/ffmpeg・~/tools/ffprobe）、BIN_DIR（例のバイナリの場所。
#           指定しなければ cargo build する）、TMPDIR（作業場所）。
# 注: FFmpeg 7 の hls demuxer は既定で .vtt の拡張子を拒むため `-allowed_extensions ALL` を付ける。
set -euo pipefail

FFMPEG=${FFMPEG:-$HOME/tools/ffmpeg}
FFPROBE=${FFPROBE:-$HOME/tools/ffprobe}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

if [[ -z "${BIN_DIR:-}" ]]; then
  CARGO=${CARGO:-cargo}
  command -v "$CARGO" >/dev/null || CARGO=$HOME/.cargo/bin/cargo
  (cd "$ROOT" && "$CARGO" build -q -p senca-caption --examples)
  BIN_DIR=${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples
fi
EX="$BIN_DIR/caption_files"

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
ff() { "$FFMPEG" -nostdin -hide_banner -v error -y "$@"; }

cd "$WORK"

# --- 素材 ---
ff -f lavfi -i sine=frequency=440:sample_rate=48000:duration=13 -c:a aac -b:a 96k aac.m4a
ff -f lavfi -i sine=frequency=440:sample_rate=48000:duration=13 -c:a libopus -b:a 64k opus.mp4

# check_chunks <名前> <dir>: 各チャンクを復号してサンプル数を比べる
check_chunks() {
  local name=$1 dir=$2 n=0 bad=0 file start dur want got err
  while IFS=$'\t' read -r file start dur; do
    n=$((n + 1))
    err=$("$FFMPEG" -nostdin -v error -i "$dir/$file" -f s16le -ac 1 -ar 48000 "$dir/$file.pcm" 2>&1) || { fail "$name $file: decode failed: $err"; bad=1; continue; }
    if [[ -n "$err" ]]; then fail "$name $file: decoder errors: $err"; bad=1; continue; fi
    got=$(($(stat -c %s "$dir/$file.pcm") / 2))
    want=$((dur * 48000 / 1000000))
    if ((got - want > 1 || want - got > 1)); then fail "$name $file: $got samples, expected $want"; bad=1; fi
  done <"$dir/chunks.tsv"
  if ((n < 3)); then fail "$name: only $n chunks"; return; fi
  ((bad == 0)) && pass "$name: $n chunks decode cleanly with the expected length"
}

"$EX" chunks aac.m4a m4a >/dev/null && check_chunks "aac -> m4a" m4a || fail "aac -> m4a (example failed)"
"$EX" chunks aac.m4a adts adts >/dev/null && check_chunks "aac -> adts" adts || fail "aac -> adts (example failed)"
"$EX" chunks opus.mp4 ogg >/dev/null && check_chunks "opus -> ogg" ogg || fail "opus -> ogg (example failed)"

fmt=$("$FFPROBE" -v error -show_entries format=format_name -of csv=p=0 ogg/chunk-0.ogg)
codec=$("$FFPROBE" -v error -show_entries stream=codec_name -of csv=p=0 ogg/chunk-0.ogg)
[[ "$fmt" == ogg && "$codec" == opus ]] && pass "ffprobe: ogg/opus" || fail "ffprobe ogg: $fmt/$codec"
fmt=$("$FFPROBE" -v error -show_entries format=format_name -of csv=p=0 m4a/chunk-0.m4a)
codec=$("$FFPROBE" -v error -show_entries stream=codec_name -of csv=p=0 m4a/chunk-0.m4a)
[[ "$fmt" == *mp4* && "$codec" == aac ]] && pass "ffprobe: mp4/aac" || fail "ffprobe m4a: $fmt/$codec"
# 重なり: 隣のチャンクとの重なりが約 1 秒
ov=$(awk -F'\t' 'NR>1 { o = pe - $2; if (o < 900000 || o > 1100000) bad = 1 } { pe = $2 + $3 } END { print bad ? "bad" : "ok" }' m4a/chunks.tsv)
[[ "$ov" == ok ]] && pass "chunks overlap by ~1 s" || fail "chunk overlap"

# --- 字幕 ---
mkdir subs
ncues=$("$EX" subs subs 6 2)
ff -f lavfi -i testsrc2=size=320x180:rate=30 -t 12 -c:v libx264 -g 60 -keyint_min 60 -sc_threshold 0 \
  -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_type fmp4 subs/video.m3u8

# 録画用の全体
out=$("$FFMPEG" -nostdin -v error -i subs/captions_ja.vtt -f webvtt - 2>"$WORK/err") || true
got=$(grep -c '^字幕 ' <<<"$out" || true)
[[ $got == "$ncues" && ! -s "$WORK/err" ]] && pass "full .vtt: $got cues" || fail "full .vtt: $got of $ncues cues ($(cat "$WORK/err"))"
grep -q '字幕 1 <テスト> & 確認' <<<"$out" && pass "escaping round-trips" || fail "escaping"

# HLS の字幕 playlist（境界をまたぐ Cue は両方のセグメントに入るので、異なる文の数で数える）
out=$("$FFMPEG" -nostdin -v error -allowed_extensions ALL -i subs/sub_ja.m3u8 -f webvtt - 2>"$WORK/err") || true
got=$(grep '^字幕 ' <<<"$out" | sort -u | wc -l)
[[ $got == "$ncues" && ! -s "$WORK/err" ]] && pass "subtitle playlist: $got distinct cues" || fail "subtitle playlist: $got of $ncues ($(cat "$WORK/err"))"

# VOD の 1 ファイル playlist
out=$("$FFMPEG" -nostdin -v error -allowed_extensions ALL -i subs/captions_ja.m3u8 -f webvtt - 2>&1) || true
got=$(grep -c '^字幕 ' <<<"$out" || true)
[[ $got == "$ncues" ]] && pass "VOD subtitle playlist: $got cues" || fail "VOD subtitle playlist: $got of $ncues"

# multivariant: FFmpeg 7.0 の hls demuxer は SUBTITLES の rendition を開かない（"Can't support the subtitle"）ので、
# 映像が読めることと、EXT-X-MEDIA を SUBTITLES の URI として認識したことを確かめる
info=$("$FFPROBE" -v verbose -allowed_extensions ALL -show_entries stream=codec_type,codec_name -of csv=p=0 subs/master.m3u8 2>&1 || true)
if grep -q 'h264,video' <<<"$info" && grep -q 'subtitle(uri: sub_ja.m3u8)' <<<"$info"; then
  pass "multivariant with EXT-X-MEDIA TYPE=SUBTITLES (video read, subtitle rendition recognized)"
else
  fail "multivariant: $(tail -5 <<<"$info")"
fi
grep -q '^#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="日本語",LANGUAGE="ja",DEFAULT=YES,AUTOSELECT=YES,URI="sub_ja.m3u8"$' subs/master.m3u8 \
  && grep -q 'SUBTITLES="subs"' subs/master.m3u8 && pass "EXT-X-MEDIA / SUBTITLES attribute" || fail "EXT-X-MEDIA line"

echo "== $PASS passed, $FAIL failed"
[[ $FAIL == 0 ]]
