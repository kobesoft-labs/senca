/*
 * SENCA SDK（C ABI、crates/senca-sdk/include/senca.h）と Kotlin（SencaNative.kt）をつなぐ JNI。
 *
 * - ハンドルは jlong（senca_sender* をそのまま）。Kotlin 側の SencaSender が寿命を管理する。
 * - フレームは MediaCodec の出力の direct ByteBuffer をコピーせずに渡す（SDK が内部でコピーする）。
 * - SDK のコールバックは SDK の送信スレッドから来るので、JVM に attach してから Kotlin を呼ぶ。
 *   attach したスレッドはスレッドの終わりに detach する（pthread の key の destructor）。
 * - 失敗は senca_last_error() の文字列を付けて SencaException を投げる。
 */
#include <jni.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>

#include "senca.h"

static JavaVM *g_vm;
static pthread_key_t g_detach_key;

static void detach_thread(void *unused) {
    (void)unused;
    if (g_vm) (*g_vm)->DetachCurrentThread(g_vm);
}

JNIEXPORT jint JNI_OnLoad(JavaVM *vm, void *reserved) {
    (void)reserved;
    g_vm = vm;
    pthread_key_create(&g_detach_key, detach_thread);
    return JNI_VERSION_1_6;
}

static void throw_senca(JNIEnv *env, int32_t code) {
    jclass cls = (*env)->FindClass(env, "jp/co/kobesoft/senca/sender/SencaException");
    if (!cls) return;
    jmethodID ctor = (*env)->GetMethodID(env, cls, "<init>", "(ILjava/lang/String;)V");
    jstring msg = (*env)->NewStringUTF(env, senca_last_error());
    jobject ex = (*env)->NewObject(env, cls, ctor, (jint)code, msg);
    (*env)->Throw(env, (jthrowable)ex);
}

/* コールバックの相手（Kotlin の SenderListener）。senca_sender ごとに 1 つ。 */
typedef struct {
    jobject listener; /* global ref */
    jmethodID on_event;
} listener_ctx;

/* ハンドルと listener をまとめて持つ */
typedef struct {
    senca_sender *sender;
    listener_ctx *ctx;
} handle_t;

static void on_event(void *user, const senca_event *ev) {
    listener_ctx *ctx = user;
    JNIEnv *env = NULL;
    if ((*g_vm)->GetEnv(g_vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if ((*g_vm)->AttachCurrentThreadAsDaemon(g_vm, &env, NULL) != JNI_OK) return;
        pthread_setspecific(g_detach_key, env);
    }
    jstring reason = ev->reason ? (*env)->NewStringUTF(env, ev->reason) : NULL;
    (*env)->CallVoidMethod(env, ctx->listener, ctx->on_event, (jint)ev->type, (jint)ev->state, reason);
    if ((*env)->ExceptionCheck(env)) (*env)->ExceptionClear(env);
    if (reason) (*env)->DeleteLocalRef(env, reason);
}

#define FN(name) Java_jp_co_kobesoft_senca_sender_SencaNative_##name
#define H(h) ((handle_t *)(intptr_t)(h))

JNIEXPORT jint JNICALL FN(abiVersion)(JNIEnv *env, jobject self) {
    (void)env;
    (void)self;
    return (jint)senca_abi_version();
}

JNIEXPORT jlong JNICALL FN(create)(JNIEnv *env, jobject self, jstring config, jobject listener) {
    (void)self;
    const char *c = (*env)->GetStringUTFChars(env, config, NULL);
    if (!c) return 0;
    senca_sender_options opt;
    memset(&opt, 0, sizeof opt);
    opt.struct_size = sizeof opt;
    senca_sender *s = NULL;
    int32_t r = senca_sender_create(c, &opt, &s);
    (*env)->ReleaseStringUTFChars(env, config, c);
    if (r != SENCA_OK) {
        throw_senca(env, r);
        return 0;
    }
    handle_t *h = calloc(1, sizeof *h);
    if (!h) {
        senca_sender_destroy(s);
        return 0;
    }
    h->sender = s;
    if (listener) {
        listener_ctx *ctx = calloc(1, sizeof *ctx);
        jclass cls = (*env)->GetObjectClass(env, listener);
        ctx->listener = (*env)->NewGlobalRef(env, listener);
        ctx->on_event = (*env)->GetMethodID(env, cls, "onEvent", "(IILjava/lang/String;)V");
        h->ctx = ctx;
        senca_sender_set_callback(s, on_event, ctx);
    }
    return (jlong)(intptr_t)h;
}

static void check(JNIEnv *env, int32_t r) {
    if (r < 0) throw_senca(env, r);
}

JNIEXPORT void JNICALL FN(start)(JNIEnv *env, jobject self, jlong h) {
    (void)self;
    check(env, senca_sender_start(H(h)->sender));
}

JNIEXPORT void JNICALL FN(setVideoConfig)(JNIEnv *env, jobject self, jlong h, jint codec, jbyteArray record) {
    (void)self;
    jsize n = (*env)->GetArrayLength(env, record);
    jbyte *b = (*env)->GetByteArrayElements(env, record, NULL);
    int32_t r = senca_sender_set_video_config(H(h)->sender, codec, (const uint8_t *)b, (size_t)n);
    (*env)->ReleaseByteArrayElements(env, record, b, JNI_ABORT);
    check(env, r);
}

JNIEXPORT void JNICALL FN(setAudioConfig)(JNIEnv *env, jobject self, jlong h, jint codec, jint rate, jint channels,
                                          jbyteArray extra) {
    (void)self;
    jsize n = extra ? (*env)->GetArrayLength(env, extra) : 0;
    jbyte *b = extra ? (*env)->GetByteArrayElements(env, extra, NULL) : NULL;
    int32_t r = senca_sender_set_audio_config(H(h)->sender, codec, (uint32_t)rate, (uint32_t)channels,
                                              (const uint8_t *)b, (size_t)n);
    if (b) (*env)->ReleaseByteArrayElements(env, extra, b, JNI_ABORT);
    check(env, r);
}

static const uint8_t *direct(JNIEnv *env, jobject buf, jint off, jint size) {
    uint8_t *p = (*env)->GetDirectBufferAddress(env, buf);
    jlong cap = (*env)->GetDirectBufferCapacity(env, buf);
    if (!p || off < 0 || size < 0 || (jlong)off + size > cap) return NULL;
    return p + off;
}

/* 戻り値: SENCA_OK / SENCA_DROPPED。エラーは例外 */
JNIEXPORT jint JNICALL FN(pushVideo)(JNIEnv *env, jobject self, jlong h, jint codec, jint format, jobject buf,
                                     jint off, jint size, jlong pts, jlong dts, jint key) {
    (void)self;
    const uint8_t *p = direct(env, buf, off, size);
    if (!p) {
        check(env, SENCA_ERR_INVALID_ARGUMENT);
        return SENCA_ERR_INVALID_ARGUMENT;
    }
    int32_t r = senca_sender_push_video(H(h)->sender, codec, format, p, (size_t)size, pts, dts, key);
    check(env, r);
    return r;
}

JNIEXPORT jint JNICALL FN(pushAudio)(JNIEnv *env, jobject self, jlong h, jint codec, jint format, jobject buf,
                                     jint off, jint size, jlong pts) {
    (void)self;
    const uint8_t *p = direct(env, buf, off, size);
    if (!p) {
        check(env, SENCA_ERR_INVALID_ARGUMENT);
        return SENCA_ERR_INVALID_ARGUMENT;
    }
    int32_t r = senca_sender_push_audio(H(h)->sender, codec, format, p, (size_t)size, pts);
    check(env, r);
    return r;
}

/* [state, bytesSent, bitrateBps, videoSent, audioSent, videoDropped, audioDropped, queueMs, rttUs, reconnects] */
JNIEXPORT jlongArray JNICALL FN(stats)(JNIEnv *env, jobject self, jlong h) {
    (void)self;
    senca_stats st;
    memset(&st, 0, sizeof st);
    st.struct_size = sizeof st;
    check(env, senca_sender_get_stats(H(h)->sender, &st));
    jlong v[10] = {st.state,
                   (jlong)st.bytes_sent,
                   (jlong)st.bitrate_bps,
                   (jlong)st.video_frames_sent,
                   (jlong)st.audio_frames_sent,
                   (jlong)st.video_frames_dropped,
                   (jlong)st.audio_frames_dropped,
                   (jlong)st.queue_duration_ms,
                   st.rtt_us,
                   (jlong)st.reconnects};
    jlongArray a = (*env)->NewLongArray(env, 10);
    if (a) (*env)->SetLongArrayRegion(env, a, 0, 10, v);
    return a;
}

JNIEXPORT void JNICALL FN(stop)(JNIEnv *env, jobject self, jlong h) {
    (void)self;
    check(env, senca_sender_stop(H(h)->sender));
}

JNIEXPORT void JNICALL FN(destroy)(JNIEnv *env, jobject self, jlong h) {
    (void)self;
    handle_t *hd = H(h);
    if (!hd) return;
    /* destroy は送信スレッドの終わりを待つので、その後なら listener を消してよい */
    senca_sender_destroy(hd->sender);
    if (hd->ctx) {
        (*env)->DeleteGlobalRef(env, hd->ctx->listener);
        free(hd->ctx);
    }
    free(hd);
}

JNIEXPORT jstring JNICALL FN(normalizeUri)(JNIEnv *env, jobject self, jstring input) {
    (void)self;
    const char *c = (*env)->GetStringUTFChars(env, input, NULL);
    if (!c) return NULL;
    char *out = NULL;
    int32_t r = senca_uri_normalize(c, &out);
    (*env)->ReleaseStringUTFChars(env, input, c);
    if (r != SENCA_OK) {
        throw_senca(env, r);
        return NULL;
    }
    jstring s = (*env)->NewStringUTF(env, out);
    senca_string_free(out);
    return s;
}

/* VISCA: [seq, kind, a, b, c, d]。知らないコマンドは kind = 0（seq は入る）、壊れたパケットは null */
JNIEXPORT jintArray JNICALL FN(viscaParse)(JNIEnv *env, jobject self, jbyteArray pkt, jint len) {
    (void)self;
    jbyte *b = (*env)->GetByteArrayElements(env, pkt, NULL);
    uint32_t seq = 0;
    senca_visca_command cmd;
    memset(&cmd, 0, sizeof cmd);
    int32_t r = senca_visca_parse((const uint8_t *)b, (size_t)len, &seq, &cmd);
    (*env)->ReleaseByteArrayElements(env, pkt, b, JNI_ABORT);
    if (r != SENCA_OK && r != SENCA_ERR_UNSUPPORTED) return NULL;
    jint v[6] = {(jint)seq, r == SENCA_OK ? cmd.kind : 0, cmd.a, cmd.b, cmd.c, cmd.d};
    jintArray a = (*env)->NewIntArray(env, 6);
    if (a) (*env)->SetIntArrayRegion(env, a, 0, 6, v);
    return a;
}

JNIEXPORT jbyteArray JNICALL FN(viscaReply)(JNIEnv *env, jobject self, jint kind, jint socket, jint error_code,
                                            jbyteArray data, jint seq) {
    (void)self;
    uint8_t buf[32];
    size_t n = 0;
    jsize dn = data ? (*env)->GetArrayLength(env, data) : 0;
    jbyte *d = data ? (*env)->GetByteArrayElements(env, data, NULL) : NULL;
    int32_t r = senca_visca_build_reply(kind, (uint32_t)socket, (uint32_t)error_code, (const uint8_t *)d, (size_t)dn,
                                        (uint32_t)seq, buf, sizeof buf, &n);
    if (d) (*env)->ReleaseByteArrayElements(env, data, d, JNI_ABORT);
    if (r != SENCA_OK) {
        throw_senca(env, r);
        return NULL;
    }
    jbyteArray a = (*env)->NewByteArray(env, (jsize)n);
    if (a) (*env)->SetByteArrayRegion(env, a, 0, (jsize)n, (const jbyte *)buf);
    return a;
}
