#!/usr/bin/env python3
"""部屋のシグナリング（GET /signaling、subprotocol senca.rooms.v1）へ connect を 1 つ送り、最初の応答を表示する。

使い方: ws_probe.py ws://127.0.0.1:PORT/signaling ROOM TOKEN [HOLD_SECS]
出力: 最初に届いたテキストの JSON（1 行）。届かなければ {"type":"none"}。標準ライブラリだけで動く（試験用の最小の実装）。
HOLD_SECS を渡すと、その秒数（または閉じられるまで）つないだままにし、届いたテキストを 1 行ずつ出す（ping には pong を返す。
閉じられたら {"type":"close","code":N}）。
"""
import base64, json, os, socket, struct, sys, urllib.parse


def recv_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


def main():
    url, room, token = sys.argv[1], sys.argv[2], sys.argv[3]
    u = urllib.parse.urlparse(url)
    s = socket.create_connection((u.hostname, u.port), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (f"GET {u.path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: senca.rooms.v1\r\n\r\n")
    s.sendall(req.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        head += s.recv(1)
    if b" 101 " not in head.split(b"\r\n")[0]:
        print(json.dumps({"type": "http", "status": head.split(b"\r\n")[0].decode()}))
        return
    msg = json.dumps({"type": "connect", "version": 1, "room_id": room, "role": "recvonly", "token": token}).encode()
    mask = os.urandom(4)
    hdr = bytes([0x81])
    n = len(msg)
    hdr += bytes([0x80 | n]) if n < 126 else (bytes([0x80 | 126]) + struct.pack("!H", n) if n < 65536 else bytes([0x80 | 127]) + struct.pack("!Q", n))
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(msg)))
    hold = float(sys.argv[4]) if len(sys.argv) > 4 else 0
    if hold:
        return hold_loop(s, hold)
    try:
        while True:
            b0, b1 = recv_exact(s, 2)
            ln = b1 & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", recv_exact(s, 2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", recv_exact(s, 8))[0]
            payload = recv_exact(s, ln)
            op = b0 & 0x0F
            if op == 1:
                print(payload.decode())
                return
            if op == 8:
                code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else None
                print(json.dumps({"type": "close", "code": code}))
                return
    except (EOFError, socket.timeout):
        print(json.dumps({"type": "none"}))


def send_text(s, text):
    msg = text.encode()
    mask = os.urandom(4)
    n = len(msg)
    hdr = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack("!H", n))
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(msg)))


def hold_loop(s, secs):
    import time
    end = time.time() + secs
    try:
        while time.time() < end:
            s.settimeout(max(0.1, end - time.time()))
            b0, b1 = recv_exact(s, 2)
            ln = b1 & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", recv_exact(s, 2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", recv_exact(s, 8))[0]
            payload = recv_exact(s, ln)
            op = b0 & 0x0F
            if op == 1:
                print(payload.decode(), flush=True)
                try:
                    if json.loads(payload).get("type") == "ping":
                        send_text(s, json.dumps({"type": "pong"}))
                except ValueError:
                    pass
            elif op == 8:
                code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else None
                print(json.dumps({"type": "close", "code": code}), flush=True)
                return
    except (EOFError, socket.timeout, OSError):
        pass
    print(json.dumps({"type": "end"}), flush=True)


if __name__ == "__main__":
    main()
