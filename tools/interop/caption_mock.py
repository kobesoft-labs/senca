#!/usr/bin/env python3
"""試験用の模擬サービス（音声認識: OpenAI 互換 /v1/audio/transcriptions、翻訳: Anthropic 互換 /v1/messages）。
外部の本物のサービスには送らない。使い方: caption_mock.py PORT"""
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

count = 0
lock = threading.Lock()

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        sys.stderr.write("%s %s\n" % (self.command, self.path))

    def reply(self, obj):
        b = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_POST(self):
        global count
        n = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(n)
        if self.path.endswith("/audio/transcriptions"):
            if b"Content-Disposition: form-data; name=\"file\"" not in body:
                self.send_response(400); self.end_headers(); return
            with lock:
                count += 1
                c = count
            self.reply({"text": f"テスト{c}", "segments": [{"id": 0, "start": 0.3, "end": 2.5, "text": f"テスト{c}です"}]})
        elif self.path.endswith("/v1/messages"):
            if self.headers.get("x-api-key") != "mock-anthropic-key":
                self.send_response(401); self.end_headers(); return
            req = json.loads(body)
            data = json.loads(req["messages"][0]["content"])
            lines = [f'{it["n"]}. EN:{it["text"]}' for it in data["items"]]
            self.reply({"content": [{"type": "text", "text": "\n".join(lines)}], "stop_reason": "end_turn"})
        else:
            self.send_response(404); self.end_headers()

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
