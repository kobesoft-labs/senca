#!/usr/bin/env python3
"""fuzz の種（seed corpus）を作る補助。tools/fuzz/seed.sh から呼ぶ。標準ライブラリだけを使う。

サブコマンド:
  text CORPUS            手書きの種（HTTP・RTSP・SDP・HLS など）
  codec CORPUS DATA...   試験データ（crates/*/tests/data）を、選択バイトを付けて codec の target へ
  rtp CORPUS FFMPEG WORK FFmpeg の RTP 出力（UDP）を受けて rtp・rtcp・rtp_depack・sdp の種にする
  ogg CORPUS FILE...     Ogg Opus から Opus パケットと OpusHead を取り出す
  adts CORPUS FILE...    ADTS から先頭のフレームを取り出す
"""

import os
import socket
import struct
import subprocess
import sys
import threading
import time


def put(corpus, target, name, data):
    d = os.path.join(corpus, target)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


# ------------------------------------------------------------------ 手書きの種

HTTP_REQS = [
    b"GET /hls/live/cam1/index.m3u8?_HLS_msn=5&_HLS_part=1&token=abc HTTP/1.1\r\nHost: a\r\nUser-Agent: x\r\nAccept: */*\r\n\r\n",
    b"POST /api/v1/streams HTTP/1.1\r\nHost: a\r\nContent-Type: application/json\r\nContent-Length: 15\r\nIdempotency-Key: k1\r\n\r\n{\"name\":\"cam\"}\n",
    b"POST /whip/x HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\nX-Trailer: 1\r\n\r\n",
    b"GET /a HTTP/1.1\r\nHost: a\r\nConnection: keep-alive\r\n\r\nGET /b HTTP/1.1\r\nHost: a\r\nRange: bytes=0-99,200-\r\n\r\n",
    b"GET /ws HTTP/1.1\r\nHost: a\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    b"GET / HTTP/1.0\r\n\r\n",
    b"PUT /x HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\nabc",
]
HTTP_RESPS = [
    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"x\"\r\n\r\nhello",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nabcd\r\n0\r\n\r\n",
    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/\r\nContent-Length: 0\r\n\r\n",
    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n",
    b"HTTP/1.0 200 OK\r\n\r\nbody until close",
]

SDP_OFFER = (
    "v=0\r\no=- 4611731400430051336 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n"
    "a=group:BUNDLE 0 1\r\na=extmap-allow-mixed\r\na=msid-semantic: WMS\r\n"
    "m=audio 9 UDP/TLS/RTP/SAVPF 111 0\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\n"
    "a=ice-ufrag:abcd\r\na=ice-pwd:aaaaaaaaaaaaaaaaaaaaaaaa\r\na=ice-options:trickle\r\n"
    "a=fingerprint:sha-256 7B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08\r\n"
    "a=setup:actpass\r\na=mid:0\r\na=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\na=sendrecv\r\n"
    "a=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\na=rtcp-fb:111 transport-cc\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n"
    "a=rtpmap:0 PCMU/8000\r\na=ssrc:1001 cname:x\r\n"
    "a=candidate:1 1 udp 2122260223 192.0.2.1 54321 typ host generation 0\r\n"
    "m=video 9 UDP/TLS/RTP/SAVPF 96 97 45\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\na=sendonly\r\na=rtcp-mux\r\na=rtcp-rsize\r\n"
    "a=rtpmap:96 H264/90000\r\na=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n"
    "a=rtcp-fb:96 nack\r\na=rtcp-fb:96 nack pli\r\na=rtcp-fb:96 ccm fir\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\n"
    "a=rtpmap:45 AV1/90000\r\na=simulcast:send f;h\r\na=rid:f send\r\na=rid:h send\r\n"
    "a=ssrc-group:FID 2001 2002\r\na=ssrc:2001 cname:x\r\na=ssrc:2002 cname:x\r\n"
)

RTSP_SDP = (
    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=cam\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\n"
    "m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"
    "a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0LAHtkDxWhAAAADAEAAAAwDxYuS,aMuMsg==\r\na=control:trackID=0\r\n"
    "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/48000/2\r\n"
    "a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190\r\n"
    "a=control:trackID=1\r\n"
)


def rtsp_session():
    sdp = RTSP_SDP.encode()
    msgs = [
        b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Base: rtsp://127.0.0.1:8554/live/cam/\r\nContent-Type: application/sdp\r\n"
        + b"Content-Length: " + str(len(sdp)).encode() + b"\r\n\r\n" + sdp,
        b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=1234ABCD\r\nSession: 12345678;timeout=60\r\n\r\n",
        b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 12345678\r\nRange: npt=0.000-\r\n"
        + b"RTP-Info: url=rtsp://127.0.0.1:8554/live/cam/trackID=0;seq=1;rtptime=0\r\n\r\n",
        b"$\x00\x00\x10\x80\x60\x00\x01\x00\x00\x00\x00\x12\x34\xab\xcd\x65\x88\x84\x00",
        b"RTSP/1.0 200 OK\r\nCSeq: 5\r\nSession: 12345678\r\nContent-Length: 0\r\n\r\n",
    ]
    return b"".join(msgs)


def rtsp_auth():
    return (
        b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Digest realm=\"cam\", nonce=\"abc\", algorithm=SHA-256, qop=\"auth\"\r\n"
        b"WWW-Authenticate: Basic realm=\"cam\"\r\n\r\n"
        b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nPublic: DESCRIBE, SETUP\r\n\r\n"
        b"RTSP/1.0 302 Moved\r\nCSeq: 3\r\nLocation: rtsp://192.0.2.5/live\r\n\r\n"
    )


RTSP_REQS = [
    b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: x\r\n\r\n",
    b"SETUP rtsp://127.0.0.1/live/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP;unicast;client_port=5000-5001,RTP/AVP/TCP;interleaved=0-1\r\n\r\n",
    b"ANNOUNCE rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Length: 10\r\n\r\nv=0\r\ns=x\r\n",
    b"PLAY rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 4\r\nSession: 1\r\nRange: clock=20260101T000000Z-\r\n"
    b"Authorization: Digest username=\"u\", realm=\"r\", nonce=\"n\", uri=\"rtsp://127.0.0.1/live\", response=\"00\"\r\n\r\n",
]

HLS = [
    "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0,CAN-SKIP-UNTIL=24.0\n"
    "#EXT-X-PART-INF:PART-TARGET=0.5\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-MAP:URI=\"i.mp4\",BYTERANGE=\"10@0\"\n"
    "#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00.000Z\n#EXT-X-PART:DURATION=0.5,URI=\"p.0\",INDEPENDENT=YES\n#EXTINF:4.0,\n"
    "#EXT-X-BYTERANGE:100@10\ns.mp4\n#EXT-X-DISCONTINUITY\n#EXT-X-GAP\n#EXTINF:4,\ng.mp4\n#EXT-X-PART:DURATION=0.5,URI=\"p.1\"\n"
    "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"p.2\"\n#EXT-X-RENDITION-REPORT:URI=\"r.m3u8\",LAST-MSN=6,LAST-PART=1\n",
    "#EXTM3U\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"n\",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"ja\",URI=\"a.m3u8\"\n"
    "#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"c\",INSTREAM-ID=\"CC1\"\n"
    "#EXT-X-STREAM-INF:BANDWIDTH=100,AVERAGE-BANDWIDTH=90,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=640x360,FRAME-RATE=30.000,AUDIO=\"a\",CLOSED-CAPTIONS=\"cc\"\nv.m3u8\n"
    "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=10,URI=\"i.m3u8\"\n",
    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.000,title\nseg0.ts\n#EXT-X-ENDLIST\n",
    "_HLS_msn=10&_HLS_part=2&_HLS_skip=YES",
]

RECORD_BAD = [
    b'{"t":"begin","schema":1,"recording":"r","stream":"s","container":"ts","started_at_ms":-9223372036854775808}\n'
    b'{"t":"segment","path":"a","epoch":18446744073709551615,"sequence":0,"duration_ms":4294967295,"bytes":18446744073709551615,"sha256":"","wall_ms":0}\n',
]


def text(corpus):
    for i, r in enumerate(HTTP_REQS):
        put(corpus, "http", f"req-{i}", bytes([0x10]) + r)
        put(corpus, "http", f"req1-{i}", bytes([0x00]) + r)
    for i, r in enumerate(HTTP_RESPS):
        put(corpus, "http", f"resp-{i}", bytes([0x11]) + r)
    put(corpus, "sdp", "webrtc-offer", SDP_OFFER.encode())
    put(corpus, "sdp", "rtsp", RTSP_SDP.encode())
    put(corpus, "sdp", "rtpmap", b"96 H264/90000")
    put(corpus, "rtsp_client", "session", bytes([0x40]) + rtsp_session())
    put(corpus, "rtsp_client", "auth", bytes([0x41]) + rtsp_auth())
    put(corpus, "rtsp_client", "auth-basic", bytes([0x43]) + rtsp_auth())
    put(corpus, "rtsp_message", "responses", bytes([0x20]) + rtsp_session() + rtsp_auth())
    for i, r in enumerate(RTSP_REQS):
        put(corpus, "rtsp_message", f"req-{i}", bytes([0x20]) + r)
    for i, h in enumerate(HLS):
        put(corpus, "hls", f"hand-{i}", h.encode())
    for i, r in enumerate(RECORD_BAD):
        put(corpus, "record", f"bad-{i}", r)
    put(corpus, "jwt", "garbage", b"\x00eyJhbGciOiJFZERTQSJ9.e30.AAAA")
    put(corpus, "srt_packet", "loss", bytes.fromhex("80030000000000000000000000000001") + bytes.fromhex("80000005000000090000000a"))


# ------------------------------------------------------------------ 試験データ

# target ごとの選択バイト（fuzz_targets/*.rs の match に合わせる）
CODEC_SEL = {
    # 拡張子: [(target, 選択バイト)]
    ".h264": [("h264", 0), ("h264", 1)],
    ".avcC": [("h264", 2)],
    ".h265": [("h265", 0), ("h265", 1)],
    ".hvcC": [("h265", 2)],
    ".obu": [("av1", 0)],
    ".av1C": [("av1", 1)],
    ".asc": [("aac", 0)],
    ".aac": [("aac", 1)],
    ".ts": [("mpegts", None)],
    ".mp4": [("fmp4", None)],
    ".m4s": [("fmp4", None)],
    ".flv": [("flv", None)],
    ".m3u8": [("hls", None)],
    ".sdp": [("sdp", None)],
    ".png": [("png", None)],
    ".jpg": [("jpeg", None)],
}
MAX_SEED = 256 * 1024


def codec(corpus, files):
    for path in files:
        ext = os.path.splitext(path)[1]
        if ext not in CODEC_SEL:
            continue
        with open(path, "rb") as f:
            data = f.read()[:MAX_SEED]
        base = os.path.basename(path)
        for target, sel in CODEC_SEL[ext]:
            payload = data if sel is None else bytes([sel]) + data
            put(corpus, target, f"{base}-{sel}", payload)


# ------------------------------------------------------------------ Ogg Opus・ADTS

def ogg_packets(data):
    pos = 0
    pkt = b""
    while pos + 27 <= len(data) and data[pos:pos + 4] == b"OggS":
        nseg = data[pos + 26]
        lacing = data[pos + 27:pos + 27 + nseg]
        p = pos + 27 + nseg
        for l in lacing:
            pkt += data[p:p + l]
            p += l
            if l < 255:
                yield pkt
                pkt = b""
        pos = p


def ogg(corpus, files):
    for path in files:
        with open(path, "rb") as f:
            data = f.read()
        base = os.path.basename(path)
        for i, p in enumerate(ogg_packets(data)):
            if i >= 12:
                break
            put(corpus, "opus", f"{base}-{i}", p)


def adts(corpus, files):
    for path in files:
        with open(path, "rb") as f:
            data = f.read()
        base = os.path.basename(path)
        put(corpus, "aac", f"{base}-head", bytes([1]) + data[:2048])
        put(corpus, "aac", f"{base}-step", bytes([7]) + data[:4096])


# ------------------------------------------------------------------ FFmpeg の RTP

# (名前, FFmpeg の入力・符号化の引数, rtp_depack の format 番号)
RTP_CODECS = [
    ("h264", ["-f", "lavfi", "-i", "testsrc=size=128x96:rate=15", "-t", "1", "-c:v", "libx264", "-g", "5",
              "-bf", "0", "-pix_fmt", "yuv420p", "-profile:v", "baseline", "-x264-params", "slice-max-size=300"], 0),
    ("h265", ["-f", "lavfi", "-i", "testsrc=size=128x96:rate=15", "-t", "1", "-c:v", "libx265", "-g", "5",
              "-x265-params", "log-level=none"], 3),
    ("vp8", ["-f", "lavfi", "-i", "testsrc=size=128x96:rate=15", "-t", "1", "-c:v", "libvpx", "-g", "5"], 4),
    ("vp9", ["-f", "lavfi", "-i", "testsrc=size=128x96:rate=15", "-t", "1", "-c:v", "libvpx-vp9", "-g", "5",
             "-deadline", "realtime", "-strict", "experimental"], 5),
    ("opus", ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "1", "-c:a", "libopus"], 7),
    ("aac", ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "1", "-ac", "2", "-c:a", "aac"], 8),
    ("pcma", ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000", "-t", "1", "-c:a", "pcm_alaw"], 10),
]


def rtp_payload(pkt):
    if len(pkt) < 12:
        return None
    cc = pkt[0] & 0x0F
    off = 12 + 4 * cc
    if pkt[0] & 0x10:
        if len(pkt) < off + 4:
            return None
        off += 4 + 4 * struct.unpack(">H", pkt[off + 2:off + 4])[0]
    end = len(pkt)
    if pkt[0] & 0x20 and end > off:
        end -= pkt[-1]
    return pkt[off:end], bool(pkt[1] & 0x80)


def rtp(corpus, ffmpeg, work):
    os.makedirs(work, exist_ok=True)
    for name, args, fmt in RTP_CODECS:
        rs = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        rs.bind(("127.0.0.1", 0))
        port = rs.getsockname()[1]
        cs = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            cs.bind(("127.0.0.1", port + 1))
        except OSError:
            cs = None
        rtp_pkts, rtcp_pkts = [], []
        stop = threading.Event()

        def recv(sock, out):
            sock.settimeout(0.2)
            while not stop.is_set():
                try:
                    out.append(sock.recv(65536))
                except socket.timeout:
                    continue

        ts = [threading.Thread(target=recv, args=(rs, rtp_pkts), daemon=True)]
        if cs:
            ts.append(threading.Thread(target=recv, args=(cs, rtcp_pkts), daemon=True))
        for t in ts:
            t.start()
        sdp = os.path.join(work, f"{name}.sdp")
        cmd = [ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-re"] + args + [
            "-f", "rtp", "-sdp_file", sdp, f"rtp://127.0.0.1:{port}?pkt_size=1200"]
        try:
            subprocess.run(cmd, check=True, timeout=60)
        except Exception as e:  # FFmpeg に符号化器がない場合など
            print(f"seed: rtp {name}: {e}", file=sys.stderr)
        time.sleep(0.3)
        stop.set()
        for t in ts:
            t.join()
        rs.close()
        if cs:
            cs.close()
        for i, p in enumerate(rtp_pkts[:16]):
            put(corpus, "rtp", f"ff-{name}-{i}", p)
        for i, p in enumerate(rtcp_pkts[:4]):
            put(corpus, "rtcp", f"ff-{name}-{i}", p)
        if os.path.exists(sdp):
            with open(sdp, "rb") as f:
                put(corpus, "sdp", f"ff-{name}", f.read())
        # rtp_depack: [format][長さ 2][印 1][payload]...
        for chunk_i in range(0, min(len(rtp_pkts), 120), 40):
            buf = bytearray([fmt])
            for p in rtp_pkts[chunk_i:chunk_i + 40]:
                r = rtp_payload(p)
                if not r:
                    continue
                pl, marker = r
                buf += struct.pack(">HB", len(pl), 1 if marker else 0) + pl
            put(corpus, "rtp_depack", f"ff-{name}-{chunk_i}", bytes(buf))
        if name == "h264":
            # 同じ payload を packetization-mode=0・sprop 付きの format でも
            for alt in (1, 2):
                buf = bytearray([alt])
                for p in rtp_pkts[:40]:
                    r = rtp_payload(p)
                    if r:
                        buf += struct.pack(">HB", len(r[0]), 1 if r[1] else 0) + r[0]
                put(corpus, "rtp_depack", f"ff-{name}-fmt{alt}", bytes(buf))


def av1_rtp(corpus, files):
    """AV1 の OBU 列を RTP の集約パケット（aggregation header + 長さ付き OBU）にする。"""

    def leb(n):
        out = bytearray()
        while True:
            b = n & 0x7F
            n >>= 7
            if n:
                out.append(b | 0x80)
            else:
                out.append(b)
                return bytes(out)

    def read_leb(d, p):
        v = 0
        for i in range(8):
            b = d[p + i]
            v |= (b & 0x7F) << (7 * i)
            if not b & 0x80:
                return v, p + i + 1
        raise ValueError

    for path in files:
        with open(path, "rb") as f:
            data = f.read()
        obus = []
        p = 0
        try:
            while p < len(data) and len(obus) < 64:
                h = data[p]
                ext = 1 if h & 0x04 else 0
                if not h & 0x02:
                    break
                size, q = read_leb(data, p + 1 + ext)
                # RTP では obu_has_size_field を落とす
                hdr = bytes([h & ~0x02]) + data[p + 1:p + 1 + ext]
                obus.append(hdr + data[q:q + size])
                p = q + size
        except (ValueError, IndexError):
            pass
        buf = bytearray([6])
        for o in obus:
            if (o[0] >> 3) & 0x0F == 2:  # temporal delimiter は送らない
                continue
            pl = bytes([0x00]) + leb(len(o)) + o  # Z=0 Y=0 W=0 N=0
            buf += struct.pack(">HB", len(pl), 1) + pl
        put(corpus, "rtp_depack", "av1-" + os.path.basename(path), bytes(buf))


def main():
    cmd = sys.argv[1]
    corpus = sys.argv[2]
    if cmd == "text":
        text(corpus)
    elif cmd == "codec":
        codec(corpus, sys.argv[3:])
    elif cmd == "rtp":
        rtp(corpus, sys.argv[3], sys.argv[4])
    elif cmd == "ogg":
        ogg(corpus, sys.argv[3:])
    elif cmd == "adts":
        adts(corpus, sys.argv[3:])
    elif cmd == "av1rtp":
        av1_rtp(corpus, sys.argv[3:])
    else:
        sys.exit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
