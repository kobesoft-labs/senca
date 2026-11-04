#!/usr/bin/env python3
"""試験用の小さな OIDC の IdP（tools/interop/auth.sh から使う）。本物の IdP の代わりに、認可コードフローの相手をする。

  oidc_mock.py <port> <rsa-private-key.pem>

- 鍵は openssl で作った RSA の秘密鍵（PEM）。ID トークンは RS256 で署名する（cryptography パッケージ）。
- /.well-known/openid-configuration・/jwks（暗号化用の鍵も 1 つ混ぜる）・/authorize・/token。
- クライアント: `senca-confidential`（シークレット `mock-client-secret`、Basic で受ける）と `senca-public`（シークレット無し）。
- /authorize は利用者に何も聞かず、/_next で決めた利用者（クレーム）でコードを出す。PKCE（S256）は必須。
- /_next（POST、JSON）: {"claims": {...}, "fault": "..."} 次の 1 回のログインの利用者と、わざと壊す所。
  fault: bad_nonce・no_nonce・bad_aud・multi_aud・expired・bad_sig・bad_iss・alg_none・unknown_kid・pkce_mismatch・deny
- /_stats（GET）: 呼ばれた回数（jwks・token）と、最後の /authorize のクエリ。
"""
import base64
import hashlib
import json
import os
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa

PORT = int(sys.argv[1])
KEY = serialization.load_pem_private_key(open(sys.argv[2], "rb").read(), password=None)
OTHER = rsa.generate_private_key(public_exponent=65537, key_size=2048)
ISSUER = f"http://127.0.0.1:{PORT}"
CLIENTS = {"senca-confidential": "mock-client-secret", "senca-public": None}
LOCK = threading.Lock()
STATE = {"next": None, "codes": {}, "stats": {"jwks": 0, "token": 0, "authorize": None}}


def b64u(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def uint(n):
    return b64u(n.to_bytes((n.bit_length() + 7) // 8, "big"))


def jwk(key, kid):
    pub = key.public_key().public_numbers()
    return {"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256", "n": uint(pub.n), "e": uint(pub.e)}


def sign(header, claims, key):
    h = b64u(json.dumps(header).encode())
    p = b64u(json.dumps(claims).encode())
    if header.get("alg") == "none":
        return f"{h}.{p}."
    sig = key.sign(f"{h}.{p}".encode(), padding.PKCS1v15(), hashes.SHA256())
    return f"{h}.{p}.{b64u(sig)}"


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body=None, headers=None):
        self.send_response(code)
        data = b""
        if body is not None:
            data = json.dumps(body).encode()
            self.send_header("Content-Type", "application/json")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(u.query))
        if u.path == "/.well-known/openid-configuration":
            return self.send(200, {
                "issuer": ISSUER,
                "authorization_endpoint": f"{ISSUER}/authorize",
                "token_endpoint": f"{ISSUER}/token",
                "jwks_uri": f"{ISSUER}/jwks",
                "response_types_supported": ["code"],
                "id_token_signing_alg_values_supported": ["RS256"],
                "token_endpoint_auth_methods_supported": ["client_secret_basic", "none"],
                "code_challenge_methods_supported": ["S256"],
            })
        if u.path == "/jwks":
            with LOCK:
                STATE["stats"]["jwks"] += 1
            enc = jwk(OTHER, "enc-1")
            enc.update({"use": "enc", "alg": "RSA-OAEP"})
            return self.send(200, {"keys": [enc, jwk(KEY, "mock-1")]})
        if u.path == "/_stats":
            with LOCK:
                return self.send(200, STATE["stats"])
        if u.path == "/authorize":
            with LOCK:
                STATE["stats"]["authorize"] = q
                nxt = STATE["next"] or {"claims": {"sub": "default-user"}}
                STATE["next"] = None
            ru = q.get("redirect_uri", "")
            ok = (q.get("response_type") == "code" and q.get("client_id") in CLIENTS and ru.startswith("http")
                  and q.get("code_challenge_method") == "S256" and len(q.get("code_challenge", "")) == 43
                  and "openid" in q.get("scope", "").split() and q.get("state") and q.get("nonce"))
            if not ok:
                return self.send(400, {"error": "invalid_request"})
            sep = "&" if "?" in ru else "?"
            if nxt.get("fault") == "deny":
                loc = f"{ru}{sep}error=access_denied&state={urllib.parse.quote(q['state'])}"
                return self.send(302, headers={"Location": loc})
            code = b64u(os.urandom(24))
            challenge = q["code_challenge"] if nxt.get("fault") != "pkce_mismatch" else b64u(hashlib.sha256(b"other").digest())
            with LOCK:
                STATE["codes"][code] = {"client_id": q["client_id"], "redirect_uri": ru, "nonce": q["nonce"],
                                        "challenge": challenge, "claims": nxt.get("claims", {}), "fault": nxt.get("fault"),
                                        "at": time.time()}
            loc = f"{ru}{sep}code={code}&state={urllib.parse.quote(q['state'])}&iss={urllib.parse.quote(ISSUER, safe='')}"
            return self.send(302, headers={"Location": loc})
        return self.send(404, {"error": "not_found"})

    def do_POST(self):
        u = urllib.parse.urlparse(self.path)
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n)
        if u.path == "/_next":
            with LOCK:
                STATE["next"] = json.loads(raw or b"{}")
            return self.send(204)
        if u.path != "/token":
            return self.send(404, {"error": "not_found"})
        with LOCK:
            STATE["stats"]["token"] += 1
        f = dict(urllib.parse.parse_qsl(raw.decode()))
        with LOCK:
            c = STATE["codes"].pop(f.get("code", ""), None)  # コードは 1 回だけ
        if c is None or f.get("grant_type") != "authorization_code" or f.get("redirect_uri") != c["redirect_uri"]:
            return self.send(400, {"error": "invalid_grant"})
        # クライアントの認証（confidential は Basic、public は client_id だけ）
        auth = self.headers.get("Authorization", "")
        if CLIENTS[c["client_id"]] is not None:
            want = "Basic " + base64.b64encode(f"{c['client_id']}:{CLIENTS[c['client_id']]}".encode()).decode()
            if auth != want:
                return self.send(401, {"error": "invalid_client"})
        elif f.get("client_id") != c["client_id"] or auth:
            return self.send(401, {"error": "invalid_client"})
        # PKCE（RFC 7636、S256）
        if b64u(hashlib.sha256(f.get("code_verifier", "").encode()).digest()) != c["challenge"]:
            return self.send(400, {"error": "invalid_grant", "error_description": "pkce"})
        now = int(time.time())
        claims = {"iss": ISSUER, "aud": c["client_id"], "iat": now, "exp": now + 300, "nonce": c["nonce"]}
        claims.update(c["claims"])
        header = {"alg": "RS256", "kid": "mock-1", "typ": "JWT"}
        key = KEY
        fault = c.get("fault")
        if fault == "bad_nonce":
            claims["nonce"] = "not-the-nonce"
        elif fault == "no_nonce":
            claims.pop("nonce")
        elif fault == "bad_aud":
            claims["aud"] = "someone-else"
        elif fault == "multi_aud":
            claims["aud"] = [c["client_id"], "someone-else"]
        elif fault == "expired":
            claims["exp"] = now - 600
            claims["iat"] = now - 900
        elif fault == "bad_sig":
            key = OTHER
        elif fault == "bad_iss":
            claims["iss"] = "http://evil.example"
        elif fault == "alg_none":
            header["alg"] = "none"
        elif fault == "unknown_kid":
            header["kid"] = "rotated-away"
        return self.send(200, {"access_token": b64u(os.urandom(16)), "token_type": "Bearer", "expires_in": 300,
                               "id_token": sign(header, claims, key)})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
