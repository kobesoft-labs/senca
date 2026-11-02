"""QUIC の相互接続試験の相手（aioquic の sans-io API）。tools/interop/quic_moq.sh から使う。

  client: python quic_peer.py client HOST PORT --ca CA [--bytes N] [--loss P] [--key-update] [--migrate] [--alpn A]
  server: python quic_peer.py server HOST PORT --cert C --key K [--alpn A]   (bidi stream の echo)

client は N バイトを送って echo を確かめ、統計を JSON で標準出力に出す（終了コード 0 で一致）。
--loss は送受信の両方向で datagram を確率 P で捨てる（自前で落とすので相手の損失回復を試せる）。
--migrate は半分届いたところで新しい UDP ソケットに移り、connection ID を変える。
"""
import argparse
import json
import random
import select
import socket
import ssl
import sys
import time

from aioquic.buffer import Buffer
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.connection import QuicConnection
from aioquic.quic.events import (ConnectionTerminated, HandshakeCompleted,
                                 StreamDataReceived)
from aioquic.quic.packet import pull_quic_header


def run_client(a):
    rnd = random.Random(a.seed)
    cfg = QuicConfiguration(is_client=True, alpn_protocols=[a.alpn], server_name=a.name)
    cfg.load_verify_locations(a.ca)
    conn = QuicConnection(configuration=cfg)
    addr = (a.host, a.port)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    conn.connect(addr, now=time.time())
    data = bytes((i * 7) % 251 for i in range(a.bytes))
    got = bytearray()
    stream = None
    sent = 0
    stats = {"dropped_in": 0, "dropped_out": 0, "migrated": False, "key_update": False}
    start = time.time()
    done = False
    while time.time() - start < 60:
        for d, to in conn.datagrams_to_send(now=time.time()):
            if a.loss and rnd.random() < a.loss:
                stats["dropped_out"] += 1
                continue
            sock.sendto(d, to)
        t = conn.get_timer()
        wait = 0.05 if t is None else max(0.0, min(0.05, t - time.time()))
        r, _, _ = select.select([sock], [], [], wait)
        if r:
            d, frm = sock.recvfrom(65536)
            if a.loss and rnd.random() < a.loss:
                stats["dropped_in"] += 1
            else:
                conn.receive_datagram(d, frm, now=time.time())
        if t is not None and time.time() >= t:
            conn.handle_timer(now=time.time())
        ev = conn.next_event()
        while ev is not None:
            if isinstance(ev, HandshakeCompleted):
                stats["alpn"] = ev.alpn_protocol
                stream = conn.get_next_available_stream_id()
            elif isinstance(ev, StreamDataReceived):
                got += ev.data
                if ev.end_stream:
                    done = True
            elif isinstance(ev, ConnectionTerminated):
                stats["terminated"] = [ev.error_code, ev.reason_phrase]
                print(json.dumps(stats))
                return 1
            ev = conn.next_event()
        if stream is not None and sent < len(data):
            chunk = data[sent:sent + 16384]
            sent += len(chunk)
            conn.send_stream_data(stream, chunk, end_stream=(sent == len(data)))
        if a.key_update and not stats["key_update"] and len(got) > len(data) // 4:
            conn.request_key_update()
            stats["key_update"] = True
        if a.migrate and not stats["migrated"] and len(got) > len(data) // 2:
            sock.close()
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.bind(("127.0.0.1", 0))
            conn.change_connection_id()
            stats["migrated"] = True
        if done:
            break
    stats["bytes"] = len(got)
    stats["match"] = bytes(got) == data
    stats["seconds"] = round(time.time() - start, 3)
    conn.close()
    for d, to in conn.datagrams_to_send(now=time.time()):
        sock.sendto(d, to)
    print(json.dumps(stats))
    return 0 if stats["match"] else 1


def run_server(a):
    cfg = QuicConfiguration(is_client=False, alpn_protocols=[a.alpn])
    cfg.load_cert_chain(a.cert, a.key)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((a.host, a.port))
    conns = {}
    print("listening", flush=True)
    while True:
        r, _, _ = select.select([sock], [], [], 0.01)
        now = time.time()
        if r:
            d, frm = sock.recvfrom(65536)
            try:
                hdr = pull_quic_header(Buffer(data=d), host_cid_length=8)
            except Exception:
                continue
            c = None
            for cc in conns.values():
                if hdr.destination_cid in [x.cid for x in cc._host_cids]:
                    c = cc
            if c is None:
                c = conns.get(hdr.destination_cid)
            if c is None:
                c = QuicConnection(configuration=cfg, original_destination_connection_id=hdr.destination_cid)
                conns[hdr.destination_cid] = c
            c.receive_datagram(d, frm, now=now)
        for c in list(conns.values()):
            t = c.get_timer()
            if t is not None and now >= t:
                c.handle_timer(now=now)
            ev = c.next_event()
            while ev is not None:
                if isinstance(ev, StreamDataReceived):
                    c.send_stream_data(ev.stream_id, ev.data, end_stream=ev.end_stream)
                ev = c.next_event()
            for d, to in c.datagrams_to_send(now=now):
                sock.sendto(d, to)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("mode")
    p.add_argument("host")
    p.add_argument("port", type=int)
    p.add_argument("--ca")
    p.add_argument("--cert")
    p.add_argument("--key")
    p.add_argument("--name", default="localhost")
    p.add_argument("--alpn", default="senca-echo")
    p.add_argument("--bytes", type=int, default=1_000_000)
    p.add_argument("--loss", type=float, default=0.0)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--key-update", action="store_true")
    p.add_argument("--migrate", action="store_true")
    a = p.parse_args()
    sys.exit(run_client(a) if a.mode == "client" else run_server(a))


main()
