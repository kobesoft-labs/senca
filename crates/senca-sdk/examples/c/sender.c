/*
 * C ABI の使用例: H.264（Annex B、AU ごとに AUD 付き、B フレームなし）と AAC（ADTS）のファイルを
 * 実時間で送る。
 *
 *   sender <config> <video.h264> <audio.aac> <fps>
 *   sender --version
 *
 * 素材の作り方（例）:
 *   ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 6 -c:v libx264 -bf 0 -g 30 \
 *          -bsf:v h264_metadata=aud=insert -f h264 v.h264
 *   ffmpeg -f lavfi -i sine=sample_rate=48000 -t 6 -c:a aac -f adts a.aac
 */
#define _POSIX_C_SOURCE 200809L
#include "senca.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ABI の構造体の大きさ（Rust 側の試験と同じ値） */
_Static_assert(sizeof(senca_stats) == 96, "senca_stats layout");
_Static_assert(sizeof(senca_sender_options) == 28, "senca_sender_options layout");
_Static_assert(sizeof(senca_visca_command) == 20, "senca_visca_command layout");

static unsigned char *read_file(const char *path, size_t *len) {
    FILE *f = fopen(path, "rb");
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *buf = n > 0 ? malloc((size_t)n) : NULL;
    if (buf && fread(buf, 1, (size_t)n, f) != (size_t)n) {
        free(buf);
        buf = NULL;
    }
    fclose(f);
    *len = buf ? (size_t)n : 0;
    return buf;
}

/* data[i..] が開始符号なら、その長さ（3 か 4） */
static size_t start_code(const unsigned char *d, size_t len, size_t i) {
    if (i + 3 <= len && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) return 3;
    if (i + 4 <= len && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1) return 4;
    return 0;
}

/* 次の AU（AUD から次の AUD の手前まで）。見つからなければ 0 */
static size_t next_au(const unsigned char *d, size_t len, size_t pos, size_t *au_len) {
    size_t sc0 = start_code(d, len, pos);
    size_t i = pos + (sc0 ? sc0 : 1);
    while (i < len) {
        size_t sc = start_code(d, len, i);
        if (sc && i + sc < len && (d[i + sc] & 0x1F) == 9) break;
        i++;
    }
    *au_len = i - pos;
    return *au_len;
}

static const unsigned adts_rates[16] = {96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050,
                                        16000, 12000, 11025, 8000,  7350,  0,     0,     0};

static int64_t now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void sleep_until(int64_t t) {
    int64_t d = t - now_us();
    if (d > 0) {
        struct timespec ts = {(time_t)(d / 1000000), (long)(d % 1000000) * 1000};
        nanosleep(&ts, NULL);
    }
}

static void on_event(void *user, const senca_event *ev) {
    (void)user;
    if (ev->type == SENCA_EVENT_STATE_CHANGED)
        fprintf(stderr, "state %d%s%s\n", ev->state, ev->reason ? ": " : "", ev->reason ? ev->reason : "");
    else if (ev->type == SENCA_EVENT_KEYFRAME_REQUESTED)
        fprintf(stderr, "keyframe requested\n");
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--version") == 0) {
        printf("senca sdk %s abi %u\n", senca_version(), senca_abi_version());
        return senca_abi_version() == SENCA_ABI_VERSION ? 0 : 1;
    }
    if (argc != 5) {
        fprintf(stderr, "usage: sender <config> <video.h264> <audio.aac> <fps>\n");
        return 2;
    }
    size_t vlen, alen;
    unsigned char *v = read_file(argv[2], &vlen);
    unsigned char *a = read_file(argv[3], &alen);
    int fps = atoi(argv[4]);
    if (!v || !a || fps <= 0) {
        fprintf(stderr, "cannot read input\n");
        return 2;
    }

    senca_sender_options opt = {0};
    opt.struct_size = sizeof opt;
    opt.reconnect_initial_ms = 200;
    senca_sender *s = NULL;
    if (senca_sender_create(argv[1], &opt, &s) != SENCA_OK) {
        fprintf(stderr, "create: %s\n", senca_last_error());
        return 1;
    }
    senca_sender_set_callback(s, on_event, NULL);
    if (senca_sender_start(s) != SENCA_OK) {
        fprintf(stderr, "start: %s\n", senca_last_error());
        return 1;
    }

    size_t vpos = 0, apos = 0;
    while (vpos < vlen && !start_code(v, vlen, vpos)) vpos++;
    uint64_t vn = 0, an = 0, dropped = 0;
    int64_t t0 = now_us() + 100000;
    for (;;) {
        int64_t vt = (int64_t)vn * 1000000 / fps;
        int64_t at = -1;
        unsigned rate = 0;
        if (apos + 7 <= alen && a[apos] == 0xFF && (a[apos + 1] & 0xF0) == 0xF0) {
            rate = adts_rates[(a[apos + 2] >> 2) & 0xF];
            if (rate) at = (int64_t)an * 1024 * 1000000 / rate;
        }
        int have_v = vpos < vlen;
        int have_a = at >= 0;
        if (!have_v && !have_a) break;
        int32_t r;
        if (have_v && (!have_a || vt <= at)) {
            size_t len;
            next_au(v, vlen, vpos, &len);
            sleep_until(t0 + vt);
            r = senca_sender_push_video(s, SENCA_CODEC_H264, SENCA_FORMAT_ANNEXB, v + vpos, len, vt, vt,
                                        SENCA_KEYFRAME_AUTO);
            vpos += len;
            vn++;
        } else {
            size_t flen = ((size_t)(a[apos + 3] & 3) << 11) | ((size_t)a[apos + 4] << 3) | (a[apos + 5] >> 5);
            if (flen < 7 || apos + flen > alen) break;
            sleep_until(t0 + at);
            r = senca_sender_push_audio(s, SENCA_CODEC_AAC, SENCA_FORMAT_ADTS, a + apos, flen, at);
            apos += flen;
            an++;
        }
        if (r == SENCA_DROPPED) {
            dropped++;
        } else if (r != SENCA_OK) {
            fprintf(stderr, "push: %s\n", senca_last_error());
            break;
        }
    }
    senca_sender_stop(s);
    senca_stats st = {0};
    st.struct_size = sizeof st;
    senca_sender_get_stats(s, &st);
    printf("video_in=%llu audio_in=%llu push_dropped=%llu video_sent=%llu audio_sent=%llu bytes=%llu state=%d\n",
           (unsigned long long)vn, (unsigned long long)an, (unsigned long long)dropped,
           (unsigned long long)st.video_frames_sent, (unsigned long long)st.audio_frames_sent,
           (unsigned long long)st.bytes_sent, st.state);
    int ok = st.state == SENCA_STATE_STOPPED && st.video_frames_sent == vn && st.audio_frames_sent == an;
    senca_sender_destroy(s);
    free(v);
    free(a);
    return ok ? 0 : 1;
}
