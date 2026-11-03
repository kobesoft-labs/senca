"""HTTP/3 の client（aioquic の sans-io の QUIC と HTTP/3、独立実装）。tools/interop/node_quic.sh から使う。

  python h3_client.py URL --ca CA [--method M] [-H 'name: value']... [--data BODY] [--out FILE] [--timeout S]

一つの接続で一つの要求を送り、応答の本文を --out に書き、status・header・長さ・かかった時間を JSON で
標準出力に出す（接続できなければ status は 0）。
"""
import argparse
import json
import select
import socket
import sys
import time
from urllib.parse import urlsplit

from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.connection import QuicConnection
from aioquic.quic.events import ConnectionTerminated, HandshakeCompleted


def main():
    p = argparse.ArgumentParser()
    p.add_argument("url")
    p.add_argument("--ca", required=True)
    p.add_argument("--method", default="GET")
    p.add_argument("-H", "--header", action="append", default=[])
    p.add_argument("--data")
    p.add_argument("--out")
    p.add_argument("--timeout", type=float, default=15)
    a = p.parse_args()
    u = urlsplit(a.url)
    host, port = u.hostname, u.port or 443
    target = (u.path or "/") + (("?" + u.query) if u.query else "")
    cfg = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, server_name="localhost")
    cfg.load_verify_locations(a.ca)
    quic = QuicConnection(configuration=cfg)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    quic.connect((host, port), now=time.time())
    h3 = None
    status, headers, body, alpn, err = 0, {}, bytearray(), None, None
    sid = None
    done = False
    start = time.time()
    while time.time() - start < a.timeout and not done:
        for d, to in quic.datagrams_to_send(now=time.time()):
            sock.sendto(d, to)
        t = quic.get_timer()
        wait = 0.05 if t is None else max(0.0, min(0.05, t - time.time()))
        r, _, _ = select.select([sock], [], [], wait)
        if r:
            d, frm = sock.recvfrom(65536)
            quic.receive_datagram(d, frm, now=time.time())
        if t is not None and time.time() >= t:
            quic.handle_timer(now=time.time())
        while True:
            ev = quic.next_event()
            if ev is None:
                break
            if isinstance(ev, HandshakeCompleted):
                alpn = ev.alpn_protocol
                h3 = H3Connection(quic)
                if a.method == "HEAD":
                    # aioquic は HEAD の応答の content-length（GET のときの長さ、RFC 9110 8.6）を本文の長さと
                    # 突き合わせて接続を切るので、HEAD のときだけ照合を外す
                    h3._check_content_length = lambda stream: None
                sid = quic.get_next_available_stream_id()
                hd = [(b":method", a.method.encode()), (b":scheme", b"https"),
                      (b":authority", f"{host}:{port}".encode()), (b":path", target.encode()),
                      (b"user-agent", b"aioquic-h3-interop")]
                for h in a.header:
                    k, v = h.split(":", 1)
                    hd.append((k.strip().lower().encode(), v.strip().encode()))
                data = a.data.encode() if a.data is not None else None
                h3.send_headers(sid, hd, end_stream=data is None)
                if data is not None:
                    h3.send_data(sid, data, end_stream=True)
            elif isinstance(ev, ConnectionTerminated):
                err = f"terminated {ev.error_code:#x} {ev.reason_phrase}"
                done = True
            if h3 is not None:
                for he in h3.handle_event(ev):
                    if isinstance(he, HeadersReceived) and he.stream_id == sid:
                        for k, v in he.headers:
                            if k == b":status":
                                status = int(v)
                            else:
                                headers[k.decode()] = v.decode(errors="replace")
                        if he.stream_ended:
                            done = True
                    elif isinstance(he, DataReceived) and he.stream_id == sid:
                        body += he.data
                        if he.stream_ended:
                            done = True
    quic.close()
    for d, to in quic.datagrams_to_send(now=time.time()):
        sock.sendto(d, to)
    if a.out:
        open(a.out, "wb").write(body)
    print(json.dumps({"status": status, "alpn": alpn, "length": len(body), "headers": headers,
                      "seconds": round(time.time() - start, 3), "error": err, "complete": done}))
    sys.exit(0 if status else 1)


if __name__ == "__main__":
    main()
