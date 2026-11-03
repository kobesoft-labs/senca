#!/usr/bin/env python3
"""部屋の Webhook（認証・イベント）の試験用の受け手。署名（Senca-Signature）を確かめる。

使い方: webhook_mock.py PORT SECRET_FILE

- POST /auth: 認証 Webhook。client_id が `mallory` なら拒否、`dave` なら役割を sendonly に上書き、他は許可。
- POST /auth-deny: いつも拒否（部屋ごとの auth_webhook_url の試験）。
- POST /events: イベント Webhook。イベントごとに最初の 1 回は 500 を返し（送り直しの試験）、2 回目からは 204。
- GET /stats: 受けた物の集計（JSON）。
署名の合わない要求は 401 を返して数える。標準ライブラリだけで動く。
"""
import hashlib
import hmac
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
SECRET = open(sys.argv[2], "rb").read().strip()
LOCK = threading.Lock()
STATS = {"auth": 0, "auth_denied": 0, "auth_overridden": 0, "auth_room_denied": 0, "bad_signature": 0,
         "events": {}, "event_attempts": {}, "event_nodes": [], "event_types": {}, "delivered": 0}


def verify(header, body):
    parts = dict(p.split("=", 1) for p in header.split(",") if "=" in p) if header else {}
    sigs = [p.split("=", 1)[1] for p in (header or "").split(",") if p.startswith("v1=")]
    t = parts.get("t")
    if not t or not sigs or abs(time.time() - int(t)) > 300:
        return False
    want = hmac.new(SECRET, f"{t}.".encode() + body, hashlib.sha256).hexdigest()
    return any(hmac.compare_digest(want, s) for s in sigs)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, obj=None):
        b = json.dumps(obj).encode() if obj is not None else b""
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path == "/stats":
            with LOCK:
                return self.reply(200, STATS)
        self.reply(404, {})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        if n > 1 << 20:
            return self.reply(413, {})
        body = self.rfile.read(n)
        if not verify(self.headers.get("Senca-Signature"), body):
            with LOCK:
                STATS["bad_signature"] += 1
            return self.reply(401, {"error": "bad signature"})
        req = json.loads(body)
        if self.path == "/auth":
            with LOCK:
                STATS["auth"] += 1
            cid = req.get("client_id")
            if req.get("type") != "connection.auth" or not req.get("token_claims"):
                return self.reply(400, {})
            if cid == "mallory":
                with LOCK:
                    STATS["auth_denied"] += 1
                return self.reply(200, {"allowed": False, "reason": "banned by the test webhook"})
            if cid == "dave":
                with LOCK:
                    STATS["auth_overridden"] += 1
                return self.reply(200, {"allowed": True, "role": "sendonly", "metadata": {"via": "webhook"}})
            return self.reply(200, {"allowed": True})
        if self.path == "/auth-deny":
            with LOCK:
                STATS["auth_room_denied"] += 1
            return self.reply(200, {"allowed": False, "reason": "room webhook says no"})
        if self.path == "/events":
            eid = self.headers.get("Senca-Webhook-Id", "")
            attempt = int(self.headers.get("Senca-Webhook-Attempt", "0"))
            with LOCK:
                STATS["event_attempts"][eid] = max(attempt, STATS["event_attempts"].get(eid, 0))
                first = eid not in STATS["events"]
                if first:
                    STATS["events"][eid] = None
            if first:
                return self.reply(500, {"error": "try again"})
            with LOCK:
                if STATS["events"][eid] is None:
                    STATS["events"][eid] = req.get("type")
                    STATS["delivered"] += 1
                    STATS["event_types"][req.get("type")] = STATS["event_types"].get(req.get("type"), 0) + 1
                    if req.get("node") not in STATS["event_nodes"]:
                        STATS["event_nodes"].append(req.get("node"))
                    if str(req.get("type", "")).startswith("room."):
                        STATS.setdefault("lifecycle", []).append([req.get("type"), req.get("room_id"), req.get("node")])
            return self.reply(204)
        self.reply(404, {})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
