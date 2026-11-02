/*
 * SENCA 送り手 SDK の C ABI（安定境界、仕様 11 章）。
 *
 * 手書きのヘッダー。Rust 側（crates/senca-sdk/src/ffi.rs）と値・構造体の並びをそろえる。
 * 互換性のない変更をしたら SENCA_ABI_VERSION を上げる。構造体は先頭の struct_size で版を見分け、
 * 将来は末尾にだけ足す（呼び出し側は sizeof(...) を入れる）。
 *
 * 約束:
 *  - 文字列は NUL 終端の UTF-8。SDK が返した文字列は senca_string_free、バッファは senca_buffer_free で返す。
 *  - 戻り値は SENCA_OK（0）、SENCA_DROPPED（1）、または負のエラー。失敗の理由は senca_last_error()
 *    （呼んだスレッドごと、そのスレッドで次に senca_* を呼ぶまで有効）。理由に stream key などの秘密は入らない。
 *  - senca_sender の関数はどのスレッドから呼んでもよい（映像と音声を別スレッドから入れてよい）。
 *    ただし senca_sender_destroy は、他のスレッドがそのハンドルを使っていないときに呼ぶ。
 *  - コールバックは SDK 内部の送信スレッドから呼ばれる。中で senca_sender_stop / destroy を呼ばないこと。
 *    senca_event と reason は呼び出しの間だけ有効。
 *  - push に渡したデータは呼び出しの間にコピーする（呼び出し後に解放してよい）。
 *  - 時刻はマイクロ秒（呼び出し側の単調な時計）。DTS は増加順に入れる。
 */
#ifndef SENCA_H
#define SENCA_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SENCA_ABI_VERSION 1u

/* 戻り値 */
#define SENCA_OK 0
#define SENCA_DROPPED 1 /* ライブ優先の規則、または設定（SPS/PPS 等）が無いため捨てた。エラーではない */
#define SENCA_ERR_INVALID_ARGUMENT (-1)
#define SENCA_ERR_INVALID_CONFIG (-2)
#define SENCA_ERR_UNSUPPORTED (-3) /* 例: WHIP（未実装）、TLS の部品が無い RTMPS */
#define SENCA_ERR_INVALID_STATE (-4)
#define SENCA_ERR_BAD_FRAME (-5)
#define SENCA_ERR_BUFFER_TOO_SMALL (-6)
#define SENCA_ERR_INTERNAL (-99)

/* コーデック */
#define SENCA_CODEC_H264 1
#define SENCA_CODEC_H265 2
#define SENCA_CODEC_AAC 3
#define SENCA_CODEC_OPUS 4

/* 形式 */
#define SENCA_FORMAT_ANNEXB 1          /* 映像: 開始符号区切り（SPS/PPS/VPS を含めてよい） */
#define SENCA_FORMAT_LENGTH_PREFIXED 2 /* 映像: 4 バイト長さ前置（AVCC/HVCC） */
#define SENCA_FORMAT_RAW 3             /* 音声: AAC raw_data_block / Opus パケット */
#define SENCA_FORMAT_ADTS 4            /* 音声: AAC ADTS（設定はヘッダーから作る） */

#define SENCA_KEYFRAME_AUTO (-1) /* IDR / IRAP の有無で決める */

/* 状態（senca_sender_state、senca_stats.state、senca_event.state） */
#define SENCA_STATE_IDLE 0
#define SENCA_STATE_CONNECTING 1
#define SENCA_STATE_CONNECTED 2
#define SENCA_STATE_RECONNECTING 3
#define SENCA_STATE_STOPPED 4
#define SENCA_STATE_FAILED 5 /* 相手が断った（鍵・パスフレーズ・権限）。つなぎ直さない */

/* イベント */
#define SENCA_EVENT_STATE_CHANGED 1
#define SENCA_EVENT_KEYFRAME_REQUESTED 2 /* 符号化器に IDR を作らせること */

typedef struct senca_sender senca_sender;

/* 0 の欄は既定値を使う */
typedef struct senca_sender_options {
    uint32_t struct_size;            /* sizeof(senca_sender_options) */
    uint32_t queue_max_ms;           /* 送信キューの長さの上限（既定 2000） */
    uint32_t queue_max_bytes;        /* 送信キューの量の上限（既定 8 MiB） */
    uint32_t connect_timeout_ms;     /* 既定 5000 */
    uint32_t reconnect_initial_ms;   /* つなぎ直しの最初の間隔（既定 500、以後倍） */
    uint32_t reconnect_max_ms;       /* 間隔の上限（既定 10000） */
    uint32_t max_reconnect_attempts; /* 0 = 無制限 */
} senca_sender_options;

typedef struct senca_event {
    uint32_t struct_size;
    int32_t type;       /* SENCA_EVENT_* */
    int32_t state;      /* SENCA_STATE_*（STATE_CHANGED のとき） */
    const char *reason; /* NULL か理由（つなぎ直し・失敗のとき） */
} senca_event;

typedef void (*senca_event_callback)(void *user_data, const senca_event *event);

typedef struct senca_stats {
    uint32_t struct_size; /* 呼び出し側が sizeof(senca_stats) を入れる */
    int32_t state;
    uint64_t bytes_sent;
    uint64_t bitrate_bps; /* 直近の送信ビットレート（約 1 秒ごとに更新） */
    uint64_t video_frames_sent;
    uint64_t audio_frames_sent;
    uint64_t video_frames_dropped;
    uint64_t audio_frames_dropped;
    uint64_t queue_frames;
    uint64_t queue_bytes;
    uint64_t queue_duration_ms;
    int64_t rtt_us; /* SRT の往復時間。分からなければ -1 */
    uint64_t reconnects;
} senca_stats;

uint32_t senca_abi_version(void);
const char *senca_version(void);
const char *senca_last_error(void);

/*
 * 送り手を作る（まだつながない）。config は次のどれか:
 *   senca://ingest?v=1&proto=srt&host=H&port=P&streamid=publish:KEY[&passphrase=...][&latency=MS][&pbkeylen=16|24|32]
 *   senca://ingest?v=1&proto=rtmp&url=rtmp://H/APP&key=KEY
 *   srt://H:P?streamid=...&passphrase=...&latency=MS
 *   rtmp://H/APP/KEY（rtmps:// は TLS の部品が組み込まれたときだけ）
 * options は NULL でよい。
 */
int32_t senca_sender_create(const char *config, const senca_sender_options *options, senca_sender **out);
/* start の前に呼ぶ。 */
int32_t senca_sender_set_callback(senca_sender *s, senca_event_callback cb, void *user_data);
/* 映像の設定レコード（AVCDecoderConfigurationRecord / HEVCDecoderConfigurationRecord）。
 * Annex B で SPS/PPS(/VPS) を AU に入れて送るなら不要。 */
int32_t senca_sender_set_video_config(senca_sender *s, int32_t codec, const uint8_t *record, size_t len);
/* 音声の設定。AAC: extra = AudioSpecificConfig（NULL なら AAC-LC として作る）、ADTS なら不要。
 * Opus: extra = OpusHead（NULL なら作る）。 */
int32_t senca_sender_set_audio_config(senca_sender *s, int32_t codec, uint32_t sample_rate, uint32_t channels,
                                      const uint8_t *extra, size_t extra_len);
int32_t senca_sender_start(senca_sender *s);
/* 1 アクセスユニット。keyframe: 1 / 0 / SENCA_KEYFRAME_AUTO。戻り値 SENCA_OK / SENCA_DROPPED / エラー */
int32_t senca_sender_push_video(senca_sender *s, int32_t codec, int32_t format, const uint8_t *data, size_t len,
                                int64_t pts_us, int64_t dts_us, int32_t keyframe);
int32_t senca_sender_push_audio(senca_sender *s, int32_t codec, int32_t format, const uint8_t *data, size_t len,
                                int64_t pts_us);
int32_t senca_sender_get_stats(const senca_sender *s, senca_stats *out);
int32_t senca_sender_state(const senca_sender *s);
/* キューの残りを送り（つながっていれば）、閉じ、送信スレッドの終わりを待つ。何度呼んでもよい。 */
int32_t senca_sender_stop(senca_sender *s);
void senca_sender_destroy(senca_sender *s);

/* 設定文字列: 検査して senca:// の正規形に / 標準の URL に（WHIP のトークンは落ちる） */
int32_t senca_uri_normalize(const char *input, char **out);
int32_t senca_uri_to_standard(const char *input, char **out);
void senca_string_free(char *s);

/* QR コード（バイト/数字モード、誤り訂正 M、型番 1〜10＝213 バイトまで）。border は静穏域（推奨 4） */
int32_t senca_qr_svg(const char *text, uint32_t border, char **out_svg);
int32_t senca_qr_png(const char *text, uint32_t scale, uint32_t border, uint8_t **out_png, size_t *out_len);
void senca_buffer_free(uint8_t *p, size_t len);

/* VISCA over IP（UDP 52381） */
#define SENCA_VISCA_PORT 52381
#define SENCA_VISCA_POWER 1             /* a: 1=on 0=off */
#define SENCA_VISCA_ZOOM_STOP 2
#define SENCA_VISCA_ZOOM_TELE 3         /* a: 速さ 0..7、-1=標準 */
#define SENCA_VISCA_ZOOM_WIDE 4         /* a: 同上 */
#define SENCA_VISCA_ZOOM_DIRECT 5       /* a: 位置 0..65535 */
#define SENCA_VISCA_FOCUS_STOP 6
#define SENCA_VISCA_FOCUS_FAR 7         /* a: 速さ */
#define SENCA_VISCA_FOCUS_NEAR 8        /* a: 速さ */
#define SENCA_VISCA_FOCUS_DIRECT 9      /* a: 位置 */
#define SENCA_VISCA_FOCUS_AUTO 10
#define SENCA_VISCA_FOCUS_MANUAL 11
#define SENCA_VISCA_FOCUS_ONE_PUSH 12
#define SENCA_VISCA_WHITE_BALANCE 13    /* a: 0=auto 1=indoor 2=outdoor 3=one push 5=manual */
#define SENCA_VISCA_EXPOSURE 14         /* a: 0=full auto 3=manual 0x0A=shutter 0x0B=iris 0x0D=bright */
#define SENCA_VISCA_PRESET_RESET 15     /* a: 0..127 */
#define SENCA_VISCA_PRESET_SET 16
#define SENCA_VISCA_PRESET_RECALL 17
#define SENCA_VISCA_PAN_TILT 18         /* a: pan 速さ 1..0x18、b: tilt 速さ 1..0x17、c: 1=左 2=右 3=止、d: 1=上 2=下 3=止 */
#define SENCA_VISCA_PAN_TILT_HOME 19
#define SENCA_VISCA_ZOOM_POSITION_INQUIRY 20
#define SENCA_VISCA_REPLY_ACK 1
#define SENCA_VISCA_REPLY_COMPLETION 2
#define SENCA_VISCA_REPLY_ERROR 3

typedef struct senca_visca_command {
    int32_t kind; /* SENCA_VISCA_* */
    int32_t a, b, c, d;
} senca_visca_command;

/* 受け取ったパケットを読む（知らないコマンドは SENCA_ERR_UNSUPPORTED、out_seq は入る） */
int32_t senca_visca_parse(const uint8_t *packet, size_t len, uint32_t *out_seq, senca_visca_command *out);
/* コマンドのパケットを作る（足りなければ SENCA_ERR_BUFFER_TOO_SMALL、*out_len に必要な長さ） */
int32_t senca_visca_build(const senca_visca_command *cmd, uint32_t seq, uint8_t *buf, size_t cap, size_t *out_len);
/* 返事のパケットを作る。COMPLETION は data（問い合わせの値、例: ズーム位置の 4 ニブル）を付けられる */
int32_t senca_visca_build_reply(int32_t kind, uint32_t socket, uint32_t error_code, const uint8_t *data,
                                size_t data_len, uint32_t seq, uint8_t *buf, size_t cap, size_t *out_len);

#ifdef __cplusplus
}
#endif

#endif /* SENCA_H */
