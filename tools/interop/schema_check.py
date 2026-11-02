#!/usr/bin/env python3
"""docs/api/openapi.yaml と実装の突き合わせ（tools/interop/openapi.sh から呼ぶ）。

  schema_check.py routes <openapi.yaml> <crates/senca-node/src/api>
      経路表（api/mod.rs の route の match）と openapi.yaml の (method, path) が過不足なく一致するか、
      各操作の x-senca-scope が handler の ctx.require(Scope::...) と一致するかを静的に確かめる。

  schema_check.py live <openapi.yaml> <http-base> <token> <rtmp-base> <ffmpeg> <src.flv>
      動いているノードに実際の要求を送り、状態コードが文書にあるか、本文が文書のスキーマに合うかを確かめる。

jsonschema パッケージは使わない（下の小さな検証器: type・required・properties・additionalProperties・
enum・const・items・oneOf/anyOf・$ref・pattern・min/max）。
"""
import http.client
import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.parse

try:
    import yaml
except ImportError:  # pragma: no cover
    print("FAIL PyYAML が要ります（python3 -c 'import yaml'）")
    sys.exit(2)

PASS = 0
FAIL = 0


def ok(msg):
    global PASS
    PASS += 1
    print(f"PASS {msg}")


def bad(msg):
    global FAIL
    FAIL += 1
    print(f"FAIL {msg}")


def check(cond, msg, why=""):
    if cond:
        ok(msg)
    else:
        bad(f"{msg}{': ' + why if why else ''}")
    return cond


# ---------------------------------------------------------------- 検証器

class Validator:
    def __init__(self, spec):
        self.spec = spec

    def resolve(self, s):
        seen = 0
        while isinstance(s, dict) and "$ref" in s:
            ref = s["$ref"]
            if not ref.startswith("#/"):
                raise ValueError(f"外部参照は扱わない: {ref}")
            node = self.spec
            for part in ref[2:].split("/"):
                node = node[part.replace("~1", "/").replace("~0", "~")]
            s = node
            seen += 1
            if seen > 50:
                raise ValueError("$ref が循環")
        return s

    @staticmethod
    def type_of(v):
        if v is None:
            return "null"
        if isinstance(v, bool):
            return "boolean"
        if isinstance(v, int):
            return "integer"
        if isinstance(v, float):
            return "number"
        if isinstance(v, str):
            return "string"
        if isinstance(v, list):
            return "array"
        if isinstance(v, dict):
            return "object"
        return "unknown"

    def errors(self, v, s, path="$"):
        s = self.resolve(s)
        if s is True or s == {} or s is None:
            return []
        if s is False:
            return [f"{path}: 何も許さない"]
        errs = []
        if "oneOf" in s:
            n = sum(1 for sub in s["oneOf"] if not self.errors(v, sub, path))
            if n != 1:
                sub_errs = [e for sub in s["oneOf"] for e in self.errors(v, sub, path)]
                errs.append(f"{path}: oneOf に {n} 個合う（{'; '.join(sub_errs[:3])}）")
        if "anyOf" in s:
            if not any(not self.errors(v, sub, path) for sub in s["anyOf"]):
                errs.append(f"{path}: anyOf のどれにも合わない")
        if "allOf" in s:
            for sub in s["allOf"]:
                errs += self.errors(v, sub, path)
        t = s.get("type")
        if t is not None:
            types = t if isinstance(t, list) else [t]
            vt = self.type_of(v)
            if not (vt in types or (vt == "integer" and "number" in types)):
                return errs + [f"{path}: 型 {vt}、期待 {types}"]
        if "const" in s and v != s["const"]:
            errs.append(f"{path}: {v!r} != const {s['const']!r}")
        if "enum" in s and v not in s["enum"]:
            errs.append(f"{path}: {v!r} は enum {s['enum']} に無い")
        if isinstance(v, str):
            if "pattern" in s and not re.search(s["pattern"], v):
                errs.append(f"{path}: {v!r} が pattern {s['pattern']} に合わない")
            if "minLength" in s and len(v) < s["minLength"]:
                errs.append(f"{path}: 短すぎる")
            if "maxLength" in s and len(v) > s["maxLength"]:
                errs.append(f"{path}: 長すぎる")
        if isinstance(v, (int, float)) and not isinstance(v, bool):
            if "minimum" in s and v < s["minimum"]:
                errs.append(f"{path}: {v} < minimum {s['minimum']}")
            if "maximum" in s and v > s["maximum"]:
                errs.append(f"{path}: {v} > maximum {s['maximum']}")
        if isinstance(v, list):
            if "items" in s:
                for i, x in enumerate(v):
                    errs += self.errors(x, s["items"], f"{path}[{i}]")
            if "minItems" in s and len(v) < s["minItems"]:
                errs.append(f"{path}: 要素が少ない")
        if isinstance(v, dict):
            for r in s.get("required", []):
                if r not in v:
                    errs.append(f"{path}: 必須の {r} が無い")
            props = s.get("properties", {})
            for k, x in v.items():
                if k in props:
                    errs += self.errors(x, props[k], f"{path}.{k}")
                else:
                    ap = s.get("additionalProperties", True)
                    if ap is False:
                        errs.append(f"{path}: 文書に無いフィールド {k}")
                    elif isinstance(ap, dict):
                        errs += self.errors(x, ap, f"{path}.{k}")
        return errs


def load_spec(p):
    with open(p, encoding="utf-8") as f:
        return yaml.safe_load(f)


METHODS = ["get", "put", "post", "delete", "patch", "head", "options"]


def api_paths(spec):
    """/api/v1 の下のパス（パス単位の servers で上書きしていないもの）。"""
    return {p: item for p, item in spec["paths"].items() if "servers" not in item}


# ---------------------------------------------------------------- 静的検査

ARM = re.compile(r'^\s*\(\s*"([A-Z]+)"\s*,\s*\[([^\]]*)\]\s*\)\s*=>\s*([a-z_]+)::([a-z_0-9]+)\(')


def parse_routes(mod_rs):
    src = open(mod_rs, encoding="utf-8").read()
    body = src[src.index("fn route(ctx: &Ctx)"):]
    body = body[: body.index("\n}\n")]
    routes = []
    for line in body.splitlines():
        m = ARM.match(line)
        if not m:
            continue
        method, segs, module, func = m.groups()
        parts = []
        for seg in [x.strip() for x in segs.split(",") if x.strip()]:
            if seg.startswith('"'):
                parts.append(seg.strip('"'))
            elif re.fullmatch(r"[a-z_][a-z_0-9]*", seg):
                parts.append("{" + seg + "}")
            else:
                raise ValueError(f"読めない区切り: {seg!r}（{line.strip()}）")
        routes.append((method, "/" + "/".join(parts), module, func))
    return routes


def handler_scope(api_dir, module, func):
    """handler の本文の最初の ctx.require(Scope::X) を返す（無ければ None）。"""
    src = open(os.path.join(api_dir, f"{module}.rs"), encoding="utf-8").read()
    m = re.search(rf"pub fn {func}\b[^{{]*\{{", src)
    if not m:
        return "?"
    rest = src[m.end():]
    end = re.search(r"\n}\n", rest)
    rest = rest[: end.start()] if end else rest
    r = re.search(r"require\(Scope::(\w+)\)", rest)
    return r.group(1) if r else None


SCOPE_NAMES = {
    "StreamRead": "stream:read", "StreamWrite": "stream:write", "Publish": "publish", "Subscribe": "subscribe",
    "RecordingRead": "recording:read", "RecordingDelete": "recording:delete", "TenantAdmin": "tenant:admin",
    "NodeAdmin": "node:admin",
}


def cmd_routes(spec_path, api_dir):
    spec = load_spec(spec_path)
    routes = parse_routes(os.path.join(api_dir, "mod.rs"))
    check(len(routes) > 20, f"経路表から {len(routes)} 本の arm を読んだ")
    in_rust = {(m, p) for m, p, _, _ in routes}
    in_yaml = set()
    for p, item in api_paths(spec).items():
        for m in METHODS:
            if m in item:
                in_yaml.add((m.upper(), p))
    for m, p in sorted(in_rust - in_yaml):
        bad(f"routes: {m} {p} が実装にあって openapi.yaml に無い")
    for m, p in sorted(in_yaml - in_rust):
        bad(f"routes: {m} {p} が openapi.yaml にあって実装に無い")
    if in_rust == in_yaml:
        ok(f"routes: 実装と openapi.yaml の {len(in_rust)} 操作が一致")

    v = Validator(spec)
    for m, p, module, func in routes:
        item = spec["paths"].get(p)
        if not item or m.lower() not in item:
            continue
        op = item[m.lower()]
        label = f"{m} {p}"
        # パスの {x} がすべて path パラメーターとして書いてあるか
        params = [v.resolve(x) for x in item.get("parameters", []) + op.get("parameters", [])]
        names = {x["name"] for x in params if x.get("in") == "path"}
        want = set(re.findall(r"\{(\w+)\}", p))
        if want != names:
            bad(f"params: {label} の path パラメーター {sorted(names)} != {sorted(want)}")
        # 権限
        if module == "system" and func == "events_unsupported":
            # SSE は HTTP 層（events.rs）で stream:read を検査する
            sc = "stream:read"
        else:
            raw = handler_scope(api_dir, module, func)
            sc = SCOPE_NAMES.get(raw) if raw else None
        if "x-senca-scope" not in op:
            bad(f"scope: {label} に x-senca-scope が無い")
        elif op["x-senca-scope"] != sc:
            bad(f"scope: {label} の x-senca-scope={op['x-senca-scope']!r}、実装は {sc!r}")
        # 共通: 401、POST は Idempotency-Key
        if "401" not in op.get("responses", {}):
            bad(f"doc: {label} に 401 が無い")
        if m == "POST" and not any(x.get("name") == "Idempotency-Key" for x in params):
            bad(f"doc: {label} に Idempotency-Key が無い")
        if not op.get("operationId"):
            bad(f"doc: {label} に operationId が無い")
    ok("routes: path パラメーター・x-senca-scope・401・Idempotency-Key を確認した") if FAIL == 0 else None
    # $ref がすべて解決できるか
    def walk(n, where):
        if isinstance(n, dict):
            if "$ref" in n:
                try:
                    v.resolve(n)
                except Exception as e:  # noqa: BLE001
                    bad(f"ref: {where}: {e}")
            for k, x in n.items():
                walk(x, f"{where}/{k}")
        elif isinstance(n, list):
            for i, x in enumerate(n):
                walk(x, f"{where}/{i}")
    before = FAIL
    walk(spec, "#")
    if FAIL == before:
        ok("refs: すべての $ref が解決できる")
    n_ops = sum(1 for item in spec["paths"].values() for m in METHODS if m in item)
    print(f"== openapi.yaml: {len(spec['paths'])} パス、{n_ops} 操作（/api/v1 は {len(in_yaml)}）")


# ---------------------------------------------------------------- 実地

class Live:
    def __init__(self, spec, base, token, rtmp, ffmpeg, src):
        self.spec = spec
        self.v = Validator(spec)
        u = urllib.parse.urlparse(base)
        self.host, self.port = u.hostname, u.port
        self.http_base = base.rstrip("/")
        self.token = token
        self.rtmp = rtmp
        self.ffmpeg = ffmpeg
        self.src = src
        self.exercised = {}  # (METHOD, template) -> set(status)
        self.api = api_paths(spec)
        self.other = {p: i for p, i in spec["paths"].items() if "servers" in i}

    # パスの雛形を探す（固定の区切りが多いものを優先）
    @staticmethod
    def match(paths, path):
        segs = path.strip("/").split("/") if path.strip("/") else []
        best = None
        for tpl in paths:
            ts = tpl.strip("/").split("/")
            if len(ts) != len(segs):
                continue
            score = 0
            for a, b in zip(ts, segs):
                if a.startswith("{") and a.endswith("}"):
                    continue
                if a != b:
                    break
                score += 1
            else:
                if best is None or score > best[0]:
                    best = (score, tpl)
        return best[1] if best else None

    def raw(self, method, path, headers=None, body=None, auth=True, token=None):
        c = http.client.HTTPConnection(self.host, self.port, timeout=30)
        h = {}
        if auth:
            h["Authorization"] = f"Bearer {token or self.token}"
        if body is not None and not isinstance(body, (bytes, str)):
            body = json.dumps(body)
            h["Content-Type"] = "application/json"
        h.update(headers or {})
        c.request(method, path, body=body, headers=h)
        r = c.getresponse()
        data = r.read()
        hdrs = {k.lower(): v for k, v in r.getheaders()}
        c.close()
        return r.status, hdrs, data

    def call(self, method, path, label, expect=None, **kw):
        """/api/v1{path} を呼んで文書と突き合わせる。(status, headers, json or bytes) を返す。"""
        full = "/api/v1" + path
        status, hdrs, data = self.raw(method, full, **kw)
        p_only = path.split("?")[0]
        self.verify(method, p_only, status, hdrs, data, label, self.api, api=True)
        if expect is not None:
            exp = expect if isinstance(expect, (list, tuple, set)) else [expect]
            check(status in exp, f"{label}: 状態 {status}", f"期待 {list(exp)}、本文 {data[:300]!r}")
        ct = hdrs.get("content-type", "").split(";")[0].strip()
        body = data
        if ct.endswith("json") and data:
            try:
                body = json.loads(data)
            except ValueError:
                pass
        return status, hdrs, body

    def call_other(self, method, url_path, label, expect=None, **kw):
        status, hdrs, data = self.raw(method, url_path, auth=False, **kw)
        self.verify(method, url_path.split("?")[0], status, hdrs, data, label, self.other, api=False)
        if expect is not None:
            exp = expect if isinstance(expect, (list, tuple, set)) else [expect]
            check(status in exp, f"{label}: 状態 {status}", f"期待 {list(exp)}")
        return status, hdrs, data

    def verify(self, method, path, status, hdrs, data, label, paths, api):
        tpl = self.match(paths, path)
        ct = hdrs.get("content-type", "").split(";")[0].strip()
        if tpl is None:
            # 文書に無いパス: /api/v1 なら problem+json の 404 であるべき
            if api:
                self.check_problem(status, hdrs, data, f"{label} (未定義のパス)", 404)
            return
        item = paths[tpl]
        op = item.get(method.lower())
        if op is None:
            if api:
                want = self.spec.get("x-senca-undefined-method-status")
                self.check_problem(status, hdrs, data, f"{label} ({method} {tpl} は未定義)", want)
            else:
                bad(f"{label}: {method} {tpl} は文書に無い")
            return
        self.exercised.setdefault((method, tpl), set()).add(status)
        resp = op.get("responses", {}).get(str(status))
        if resp is None:
            bad(f"{label}: {method} {tpl} の状態 {status} が文書に無い（本文 {data[:200]!r}）")
            return
        resp = self.v.resolve(resp)
        errs = []
        # ヘッダー（required: true のものは必ず、それ以外は付いていれば形を見る）
        for hn, hs in (resp.get("headers") or {}).items():
            hs = self.v.resolve(hs)
            hv = hdrs.get(hn.lower())
            if hv is None:
                if hs.get("required"):
                    errs.append(f"ヘッダー {hn} が無い")
                continue
            hsch = hs.get("schema")
            if hsch:
                val = int(hv) if self.v.resolve(hsch).get("type") == "integer" and hv.isdigit() else hv
                errs += [f"ヘッダー {hn}: {e}" for e in self.v.errors(val, hsch, hn)]
        content = resp.get("content")
        if not content:
            if data:
                errs.append(f"本文は無いはず（{len(data)} バイト、{ct}）")
        else:
            if ct not in content:
                errs.append(f"Content-Type {ct!r} が文書に無い（{list(content)}）")
            else:
                sch = content[ct].get("schema")
                if ct.endswith("json"):
                    try:
                        j = json.loads(data)
                    except ValueError as e:
                        errs.append(f"JSON でない: {e}")
                        j = None
                    if j is not None and sch:
                        errs += self.v.errors(j, sch)
                        if ct.endswith("json") and status >= 400 and isinstance(j, dict) and j.get("status") != status:
                            errs.append(f"problem の status {j.get('status')} != {status}")
                elif sch and self.v.resolve(sch).get("const") is not None:
                    errs += self.v.errors(data.decode("utf-8", "replace"), sch)
        if errs:
            bad(f"{label}: {method} {tpl} {status} が文書に合わない: " + "; ".join(errs[:6]))
        else:
            ok(f"{label}: {method} {tpl} {status} {ct or '(本文なし)'} が文書どおり")

    def check_problem(self, status, hdrs, data, label, want):
        errs = []
        if status != want:
            errs.append(f"状態 {status}、期待 {want}")
        ct = hdrs.get("content-type", "").split(";")[0].strip()
        if ct != "application/problem+json":
            errs.append(f"Content-Type {ct}")
        else:
            j = json.loads(data)
            errs += self.v.errors(j, {"$ref": "#/components/schemas/Problem"})
        if "x-request-id" not in hdrs:
            errs.append("X-Request-Id が無い")
        check(not errs, label, "; ".join(errs))

    # ------------------------------------------------------------ 場面

    def run(self):
        t0 = time.time()
        self.health()
        self.auth()
        self.system()
        ro, sub_tok, sw = self.tokens()
        sid, other = self.streams(ro)
        rec_ids = self.publish_and_record(sid)
        self.recordings(sid, rec_ids, sw)
        self.schedules(sid)
        self.captions()
        self.io(ro)
        self.pipelines(ro)
        self.events(sub_tok)
        self.methods()
        self.cleanup(sid, other, ro)
        self.coverage()
        print(f"== live: {time.time() - t0:.1f} 秒")

    def health(self):
        self.call_other("GET", "/healthz", "health", 200)
        self.call_other("GET", "/readyz", "ready", 200)

    def auth(self):
        s, h, _ = self.call("GET", "/streams", "auth なし", 401, auth=False)
        s, h, _ = self.call("GET", "/streams", "auth 不明なトークン", 401, token="senca_bogus")
        s, h, _ = self.call("GET", "/streams", "auth Basic", 401, headers={"Authorization": "Basic eDp5"}, auth=False)
        s, h, _ = self.call("GET", "/capabilities", "X-Request-Id の引き継ぎ", 200, headers={"X-Request-Id": "abc-123"})
        check(h.get("x-request-id") == "abc-123", "X-Request-Id を引き継ぐ", h.get("x-request-id"))
        check(h.get("cache-control") == "no-store", "Cache-Control: no-store")
        s, h, b = self.call("GET", "/streams?limit=0", "X-Request-Id が problem に入る", 400, headers={"X-Request-Id": "rid-9"})
        check(isinstance(b, dict) and b.get("request_id") == "rid-9", "problem.request_id = X-Request-Id")

    def system(self):
        s, _, caps = self.call("GET", "/capabilities", "capabilities", 200)
        s, _, nodes = self.call("GET", "/nodes", "nodes", 200)
        nid = nodes["items"][0]["id"] if s == 200 and nodes.get("items") else "x"
        self.call("GET", f"/nodes/{nid}", "node", 200)
        self.call("GET", "/nodes/nosuch", "node 無い", 404)
        s, _, n = self.call("POST", f"/nodes/{nid}/drain", "drain", 202)
        check(isinstance(n, dict) and n.get("draining") is True, "drain で draining=true")
        s, _, n = self.call("POST", f"/nodes/{nid}/undrain", "undrain", 202)
        check(isinstance(n, dict) and n.get("draining") is False, "undrain で draining=false")
        self.call("POST", "/nodes/nosuch/drain", "drain 無い", 404)
        self.call("GET", "/devices", "devices", 200)

    def tokens(self):
        s, h, t = self.call("POST", "/tokens", "token 発行 stream:read", 201, body={"name": "ro", "scopes": ["stream:read"]})
        ro = t["token"]
        check(h.get("location", "").startswith("/api/v1/tokens/"), "token の Location")
        self.ro_id = t["id"]
        s, _, t2 = self.call("POST", "/tokens", "token 発行 subscribe", 201,
                             body={"name": "sub", "scopes": ["subscribe"], "expires_at": "2099-01-01T00:00:00Z"})
        sub_tok = t2["token"]
        s, _, t3 = self.call("POST", "/tokens", "token 発行 stream:write", 201, body={"name": "sw", "scopes": ["stream:write", "recording:read"]})
        sw = t3["token"]
        self.call("POST", "/tokens", "token 知らない権限", 400, body={"name": "x", "scopes": ["root"]})
        self.call("POST", "/tokens", "token 権限が空", 400, body={"name": "x", "scopes": []})
        self.call("POST", "/tokens", "token 名前が空", 400, body={"name": "", "scopes": ["stream:read"]})
        self.call("POST", "/tokens", "token 期限が過去", 400, body={"name": "x", "scopes": ["stream:read"], "expires_at": "2000-01-01T00:00:00Z"})
        self.call("POST", "/tokens", "token 期限の形", 400, body={"name": "x", "scopes": ["stream:read"], "expires_at": "tomorrow"})
        self.call("POST", "/tokens", "token 必須が無い", 400, body={"name": "x"})
        self.call("POST", "/tokens", "token stream:read では発行できない", 403, token=ro, body={"name": "x", "scopes": ["stream:read"]})
        s, h, r1 = self.call("POST", "/tokens", "token Idempotency 1 回目", 201,
                             headers={"Idempotency-Key": "tok-1"}, body={"name": "idem", "scopes": ["stream:read"]})
        s, h, r2 = self.call("POST", "/tokens", "token Idempotency 再送", 201,
                             headers={"Idempotency-Key": "tok-1"}, body={"name": "idem", "scopes": ["stream:read"]})
        check(h.get("idempotency-replayed") == "true" and r2.get("token") is None and r2.get("token_redacted") is True
              and r2.get("id") == r1.get("id"), "token 再送は同じ id で値を伏せる")
        s, _, lst = self.call("GET", "/tokens?limit=2", "tokens 一覧", 200)
        check(isinstance(lst, dict) and all(x["token"] is None for x in lst["items"]), "一覧に値が無い")
        if lst.get("next_cursor"):
            self.call("GET", f"/tokens?limit=2&cursor={lst['next_cursor']}", "tokens 次のページ", 200)
        self.call("GET", "/tokens", "tokens stream:read では 403", 403, token=ro)
        s, _, tmp = self.call("POST", "/tokens", "token 発行（消す用）", 201, body={"name": "tmp", "scopes": ["publish"]})
        self.call("DELETE", f"/tokens/{tmp['id']}", "token 削除", 204)
        self.call("DELETE", f"/tokens/{tmp['id']}", "token 削除 2 回目", 404)
        s, _, lst = self.call("GET", "/tokens?limit=1000", "tokens 全部", 200)
        boot = [x for x in lst["items"] if x["name"] == "bootstrap-admin"]
        if boot:
            self.call("DELETE", f"/tokens/{boot[0]['id']}", "最後の管理者トークンは消せない", 409)
        return ro, sub_tok, sw

    def streams(self, ro):
        body = {
            "name": "pub", "latency_profile": "low", "codec_profile": "any",
            "recording": {"enabled": True, "retention_days": 30},
            "snapshots": {"interval_secs": 60, "keep": 10},
            "hls": {"segment_ms": 1000, "window": 6, "container": "ts"},
            "limits": {"max_viewers": 100, "max_ingress_bps": 20000000},
            "metadata": {"room": "a"},
        }
        s, h, st = self.call("POST", "/streams", "stream 作成", 201, body=body)
        sid = st["id"]
        self.key = st["ingest"]["stream_key"]
        check(h.get("location") == f"/api/v1/streams/{sid}" and h.get("etag") == '"1"', "stream の Location・ETag")
        check(isinstance(self.key, str) and self.key.startswith("sk_"), "作成の応答に stream_key")
        s, _, o = self.call("POST", "/streams", "stream 作成（royalty_free fmp4、stopped）", 201,
                            body={"name": "other", "codec_profile": "royalty_free", "hls": {"container": "fmp4"},
                                  "desired_state": "stopped", "snapshots": None})
        other = o["id"]
        self.call("POST", "/streams", "stream 名前が重複", 409, body={"name": "pub"})
        self.call("POST", "/streams", "stream 名前の形", 400, body={"name": ".x"})
        self.call("POST", "/streams", "stream 未知のフィールド", 400, body={"name": "y", "bogus": 1})
        self.call("POST", "/streams", "stream 入れ子の未知のフィールド", 400, body={"name": "y", "hls": {"foo": 1}})
        self.call("POST", "/streams", "stream hls.segment_ms 範囲外", 400, body={"name": "y", "hls": {"segment_ms": 1}})
        self.call("POST", "/streams", "stream royalty_free + ts", 400, body={"name": "y", "codec_profile": "royalty_free"})
        self.call("POST", "/streams", "stream desired_state の値", 400, body={"name": "y", "desired_state": "paused"})
        self.call("POST", "/streams", "stream 本文が JSON でない", 400, body=b"{", headers={"Content-Type": "application/json"})
        self.call("POST", "/streams", "stream Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/streams", "stream stream:read では作れない", 403, token=ro, body={"name": "z"})
        # Idempotency
        s, h1, r1 = self.call("POST", "/streams", "stream Idempotency 1 回目", 201, headers={"Idempotency-Key": "k1"}, body={"name": "idem"})
        s, h2, r2 = self.call("POST", "/streams", "stream Idempotency 再送", 201, headers={"Idempotency-Key": "k1"}, body={"name": "idem"})
        check(h2.get("idempotency-replayed") == "true" and r2.get("id") == r1.get("id") and h2.get("location") == h1.get("location"),
              "再送は同じ id・Location と Idempotency-Replayed")
        check(r2["ingest"]["stream_key"] is None and r2["ingest"].get("stream_key_redacted") is True, "再送は stream_key を伏せる")
        self.idem = r1["id"]
        self.call("POST", "/streams", "stream 同じ鍵で別の本文", 409, headers={"Idempotency-Key": "k1"}, body={"name": "idem2"})
        self.call("POST", "/streams", "Idempotency-Key の形", 400, headers={"Idempotency-Key": "a b"}, body={"name": "idem3"})
        # 失敗の再送（覚えた 4xx を返す）
        s, h, e1 = self.call("POST", "/streams", "失敗の Idempotency 1 回目", 400, headers={"Idempotency-Key": "kbad"}, body={"name": ".bad"})
        s, h, e2 = self.call("POST", "/streams", "失敗の Idempotency 再送", 400, headers={"Idempotency-Key": "kbad"}, body={"name": ".bad"})
        # 一覧
        s, _, l1 = self.call("GET", "/streams?limit=1", "streams 1 件", 200)
        check(isinstance(l1, dict) and len(l1["items"]) == 1 and l1["next_cursor"], "limit=1 で next_cursor")
        if isinstance(l1, dict) and l1.get("next_cursor"):
            s, _, l2 = self.call("GET", f"/streams?limit=1&cursor={l1['next_cursor']}", "streams 次のページ", 200)
            check(l2["items"] and l2["items"][0]["id"] > l1["items"][0]["id"], "カーソルで次へ進む")
        self.call("GET", "/streams", "streams stream:read", 200, token=ro)
        self.call("GET", "/streams?limit=1001", "streams limit 範囲外", 400)
        self.call("GET", "/streams?foo=1", "streams 知らないクエリ", 400)
        self.call("GET", "/streams?stream=x", "streams stream で絞れない", 400)
        # 取得・更新
        s, h, g = self.call("GET", f"/streams/{sid}", "stream 取得", 200)
        check(g["ingest"]["stream_key"] is None, "取得の応答に stream_key は無い")
        self.call("GET", "/streams/nosuch", "stream 無い", 404)
        self.call("PATCH", f"/streams/{sid}", "PATCH If-Match 無し", 428, body={"name": "pub"})
        self.call("PATCH", f"/streams/{sid}", "PATCH If-Match 古い", 412, headers={"If-Match": '"9"'}, body={"name": "pub"})
        self.call("PATCH", f"/streams/{sid}", "PATCH If-Match の形", 412, headers={"If-Match": "1"}, body={"name": "pub"})
        self.call("PATCH", f"/streams/{sid}", "PATCH 未知のフィールド", 400, headers={"If-Match": '"1"'}, body={"x": 1})
        self.call("PATCH", f"/streams/{sid}", "PATCH 値の範囲", 400, headers={"If-Match": '"1"'}, body={"hls": {"window": 1}})
        self.call("PATCH", f"/streams/{sid}", "PATCH 名前の重複", 409, headers={"If-Match": '"1"'}, body={"name": "other"})
        self.call("PATCH", f"/streams/{sid}", "PATCH Content-Type 違い", 415, headers={"If-Match": '"1"', "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/streams/nosuch", "PATCH 無い", 404, headers={"If-Match": '"1"'}, body={})
        self.call("PATCH", f"/streams/{sid}", "PATCH stream:read", 403, token=ro, headers={"If-Match": '"1"'}, body={})
        s, h, p = self.call("PATCH", f"/streams/{sid}", "PATCH 反映（仕様は 202、実装は 200）", 200,
                            headers={"If-Match": 'W/"1"'}, body={"metadata": {"room": "b"}, "snapshots": None})
        check(h.get("etag") == '"2"' and p.get("snapshots") is None, "PATCH で generation が進み snapshots が消える")
        s, h, k = self.call("POST", f"/streams/{sid}/keys", "鍵の作り直し", 200)
        if s == 200:
            self.key = k["ingest"]["stream_key"]
        self.call("POST", "/streams/nosuch/keys", "鍵の作り直し 無い", 404)
        # 再生 URL・計画・静止画
        self.call("POST", f"/streams/{sid}/playback-url", "再生 URL（本文なし）", 200)
        self.call("POST", f"/streams/{sid}/playback-url", "再生 URL ttl", 200, body={"ttl_secs": 60})
        self.call("POST", f"/streams/{sid}/playback-url", "再生 URL ttl=0", 400, body={"ttl_secs": 0})
        self.call("POST", f"/streams/{sid}/playback-url", "再生 URL Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", f"/streams/{sid}/playback-url", "再生 URL stream:read には subscribe が無い", 403, token=ro)
        self.call("POST", "/streams/nosuch/playback-url", "再生 URL 無い", 404)
        s, _, pl = self.call("POST", f"/streams/{sid}/plan", "計画（publish 前）", 200)
        check(pl.get("state") == "provisional", "publish 前の計画は provisional")
        self.call("POST", "/streams/nosuch/plan", "計画 無い", 404)
        self.call("GET", f"/streams/{sid}/snapshot", "静止画（配信していない）", 409)
        self.call("GET", f"/streams/{sid}/snapshot?format=gif", "静止画 format", 400)
        self.call("GET", f"/streams/{sid}/snapshots", "静止画の一覧", 200)
        self.call("GET", "/streams/nosuch/snapshots", "静止画の一覧 無い", 404)
        self.call("GET", f"/streams/{sid}/snapshots/00000000000000000000000000000000", "静止画 無い", 404)
        self.call("DELETE", f"/streams/{sid}/snapshots/00000000000000000000000000000000", "静止画の削除 無い", 404)
        self.call("POST", f"/streams/{other}/recordings", "録画 stopped の stream", 409)
        self.call("POST", "/streams/nosuch/recordings", "録画 無い stream", 404)
        self.call("POST", f"/streams/{sid}/recordings", "録画 retention_days 範囲外", 400, body={"retention_days": 0})
        return sid, other

    def publish_and_record(self, sid):
        url = f"{self.rtmp}/live/{self.key}"
        p = subprocess.Popen([self.ffmpeg, "-v", "error", "-re", "-i", self.src, "-c", "copy", "-f", "flv", url],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        rec_ids = []
        try:
            live = False
            for _ in range(60):
                time.sleep(0.2)
                s, _, g = self.call("GET", f"/streams/{sid}", "配信中の stream（待ち）")
                if s == 200 and g.get("observed_state") == "live":
                    live = True
                    break
            check(live, "publish で observed_state=live")
            time.sleep(2.5)
            self.call("GET", f"/streams/{sid}", "配信中の stream", 200)
            s, _, pl = self.call("POST", f"/streams/{sid}/plan", "計画（配信中）", 200)
            check(pl.get("state") == "ready", "配信中の計画は ready")
            self.call("GET", f"/streams/{sid}/snapshot", "静止画（配信中）", [200, 409, 422, 503])
            self.call("GET", f"/streams/{sid}/snapshot?format=png", "静止画 png（配信中）", [200, 409, 422, 503])
            s, h, r = self.call("POST", f"/streams/{sid}/recordings", "録画の開始", 202, body={"name": "manual", "retention_days": 7})
            if s == 202:
                rec_ids.append(r["id"])
                check(h.get("location") == f"/api/v1/recordings/{r['id']}", "録画の Location")
            s, _, pb = self.call("POST", f"/streams/{sid}/playback-url", "再生 URL（配信中）", 200)
            if s == 200:
                path = urllib.parse.urlparse(pb["playback"]["hls"]).path
                self.call_other("GET", path, "ライブ HLS プレイリスト", 200)
                st, _, pl_data = self.call_other("GET", path + "?_HLS_msn=0", "ライブ HLS ブロッキング", [200, 400, 503])
                self.call_other("GET", path + "?_HLS_msn=x", "ライブ HLS _HLS_msn の形", 400)
                st, _, m3u8 = self.raw("GET", path, auth=False)
                segs = [x for x in m3u8.decode().splitlines() if x and not x.startswith("#")]
                if segs:
                    base = path.rsplit("/", 1)[0]
                    self.call_other("GET", f"{base}/{segs[0]}", "ライブ HLS セグメント", 200)
                self.call_other("GET", path.rsplit("/", 1)[0] + "/nosuch.ts", "ライブ HLS 無いセグメント", 404)
                self.call_other("GET", path.rsplit("/", 1)[0] + "/master.m3u8", "ライブ 字幕の master（captions なし）", 404)
                parts = path.split("/")
                parts[3] = "AAAAAAAAAAAAAAAAAAAAAA"
                self.call_other("GET", "/".join(parts), "ライブ HLS 署名違い", 403)
                parts = path.split("/")
                parts[2] = "1"
                self.call_other("GET", "/".join(parts), "ライブ HLS 期限切れ", 403)
        finally:
            p.wait(timeout=60)
        return rec_ids

    def wait_rec(self, rid, states, secs=15):
        for _ in range(int(secs * 10)):
            s, _, b = self.raw("GET", f"/api/v1/recordings/{rid}")
            if s == 200 and json.loads(b)["state"] in states:
                return True
            time.sleep(0.1)
        return False

    def recordings(self, sid, rec_ids, sw):
        s, _, lst = self.call("GET", f"/recordings?stream={sid}", "録画の一覧（stream で絞る）", 200)
        auto = [r["id"] for r in lst.get("items", []) if r["id"] not in rec_ids] if s == 200 else []
        check(len(auto) >= 1, "recording.enabled で自動の録画ができた", f"{lst}")
        self.call("GET", "/recordings?limit=1", "録画の一覧 limit", 200)
        self.call("GET", "/recordings?state=finalized", "録画の一覧 state（無視される）", 200)
        self.call("GET", "/recordings?bogus=1", "録画の一覧 知らないクエリ", 400)
        self.call("GET", "/recordings/nosuch", "録画 無い", 404)
        # 手動の録画を止める
        for rid in rec_ids:
            s, _, r = self.call("GET", f"/recordings/{rid}", "手動の録画", 200)
            self.call("POST", f"/recordings/{rid}/stop", "録画の停止", [202, 409])
            check(self.wait_rec(rid, ["finalized", "interrupted"]), "手動の録画が確定した")
            self.call("POST", f"/recordings/{rid}/stop", "録画の停止 2 回目", 409)
        self.call("POST", "/recordings/nosuch/stop", "録画の停止 無い", 404)
        if not auto:
            return
        rid = auto[0]
        check(self.wait_rec(rid, ["finalized", "interrupted"]), "自動の録画が確定した")
        s, h, r = self.call("GET", f"/recordings/{rid}", "録画の取得", 200)
        etag = h.get("etag", '"1"')
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH If-Match 無し", 428, body={"name": "n"})
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH 古い", 412, headers={"If-Match": '"999"'}, body={"name": "n"})
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH 未知のフィールド", 400, headers={"If-Match": etag}, body={"x": 1})
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH retention_until の形", 400, headers={"If-Match": etag}, body={"retention_until": "soon"})
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH name 長すぎ", 400, headers={"If-Match": etag}, body={"name": "x" * 129})
        self.call("PATCH", f"/recordings/{rid}", "録画 PATCH Content-Type 違い", 415, headers={"If-Match": etag, "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/recordings/nosuch", "録画 PATCH 無い", 404, headers={"If-Match": etag}, body={})
        s, h, r = self.call("PATCH", f"/recordings/{rid}", "録画 PATCH 保全", 200, headers={"If-Match": etag},
                            body={"name": "auto", "legal_hold": True, "retention_until": "2099-01-01T00:00:00Z"})
        etag = h.get("etag", etag)
        self.call("DELETE", f"/recordings/{rid}", "保全中の録画は消せない", 409)
        self.call("PATCH", f"/recordings/{rid}", "保全の解除に recording:delete が要る", 403, token=sw,
                  headers={"If-Match": etag}, body={"legal_hold": False})
        s, h, r = self.call("PATCH", f"/recordings/{rid}", "録画 PATCH 保全の解除", 200, headers={"If-Match": etag},
                            body={"legal_hold": False, "retention_until": None})
        etag = h.get("etag", etag)
        s, _, pb = self.call("POST", f"/recordings/{rid}/playback-url", "VOD 再生 URL", 200)
        self.call("POST", f"/recordings/{rid}/playback-url", "VOD 再生 URL ttl 上限超え", 400, body={"ttl_secs": 10 ** 9})
        self.call("POST", "/recordings/nosuch/playback-url", "VOD 再生 URL 無い", 404)
        if s == 200:
            path = urllib.parse.urlparse(pb["playback"]["hls"]).path
            st, _, m3u8 = self.call_other("GET", path, "VOD プレイリスト", 200)
            check(b"#EXT-X-ENDLIST" in m3u8, "確定した録画の VOD に ENDLIST")
            segs = [x for x in m3u8.decode().splitlines() if x and not x.startswith("#")]
            base = path.rsplit("/", 1)[0]
            if segs:
                self.call_other("GET", f"{base}/{segs[0]}", "VOD セグメント", 200)
                self.call_other("GET", f"{base}/{segs[0]}", "VOD セグメント Range", 206, headers={"Range": "bytes=0-187"})
                self.call_other("GET", f"{base}/{segs[0]}", "VOD セグメント Range 範囲外", 416, headers={"Range": "bytes=999999999-"})
            self.call_other("GET", f"{base}/9-9.ts", "VOD 無いセグメント", 404)
        # 代表画は確定後に別スレッドで作る
        for _ in range(30):
            s, _, _ = self.raw("GET", f"/api/v1/recordings/{rid}/thumbnail")
            if s == 200:
                break
            time.sleep(0.1)
        self.call("GET", f"/recordings/{rid}/thumbnail", "代表画", [200, 404])
        s, h, mp4 = self.call("GET", f"/recordings/{rid}/export.mp4", "MP4 書き出し", 200)
        check(s == 200 and mp4[4:8] == b"ftyp", "書き出しは MP4（ftyp）")
        self.call("GET", "/recordings/nosuch/export.mp4", "MP4 書き出し 無い", 404)
        self.call("GET", "/recordings/nosuch/thumbnail", "代表画 無い", 404)
        self.call("GET", f"/recordings/{rid}", "録画 recording:read が無い", 403,
                  token=self.mk_token("norec", ["stream:read"]))
        self.call("DELETE", f"/recordings/{rid}", "録画の削除 古い If-Match", 412, headers={"If-Match": '"999"'})
        s, _, d = self.call("DELETE", f"/recordings/{rid}", "録画の削除（202）", 202, headers={"If-Match": etag})
        self.call("GET", f"/recordings/{rid}", "削除した録画は 404", 404)
        self.call("DELETE", f"/recordings/{rid}", "録画の削除 2 回目", 404)

    def mk_token(self, name, scopes):
        s, _, b = self.raw("POST", "/api/v1/tokens", body={"name": name, "scopes": scopes})
        return json.loads(b)["token"]

    @staticmethod
    def ts(off_s):
        return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + off_s))

    def schedules(self, sid):
        base = {"stream": sid, "action": "record", "start_at": self.ts(3600), "end_at": self.ts(7200)}
        self.call("POST", "/schedules/check", "予約の検査", 200, body=base)
        s, h, sc = self.call("POST", "/schedules", "予約の作成", 201, body=dict(base, name="a", pre_roll_ms=30000, post_roll_ms=1000, retention_days=3))
        scid = sc["id"]
        check(h.get("location") == f"/api/v1/schedules/{scid}" and h.get("etag") == '"1"', "予約の Location・ETag")
        s, _, c = self.call("POST", "/schedules/check", "予約の検査（重なりあり）", 200, body=base)
        check(c.get("shared_input") is True and len(c.get("overlaps", [])) == 1, "重なりと入力の共有を返す")
        rec = dict(base, recurrence={"freq": "weekly", "weekdays": [1, 3], "timezone": "Asia/Tokyo", "count": 4})
        self.call("POST", "/schedules", "予約 繰り返し（Asia/Tokyo）", 201, body=rec)
        for i in range(2):
            self.call("POST", "/schedules", f"予約 重なり {i + 3} 本目", 201, body=base)
        s, _, e = self.call("POST", "/schedules", "予約 同時 5 本目は 409", 409, body=base)
        check(isinstance(e, dict) and e.get("details", {}).get("limit") == 4, "409 の details に limit・overlaps")
        self.call("POST", "/schedules/check", "予約の検査 同時 5 本目は 409", 409, body=base)
        self.call("POST", "/schedules", "予約 end_at が start_at より前", 400, body=dict(base, end_at=self.ts(1800)))
        self.call("POST", "/schedules", "予約 窓が 24 時間超え", 400, body=dict(base, end_at=self.ts(3600 + 86400 + 60)))
        self.call("POST", "/schedules", "予約 終了が過去", 400, body=dict(base, start_at=self.ts(-7200), end_at=self.ts(-3600)))
        self.call("POST", "/schedules", "予約 時刻の形", 400, body=dict(base, start_at="tomorrow"))
        self.call("POST", "/schedules", "予約 snapshot に間隔が無い", 400, body=dict(base, action="snapshot"))
        self.call("POST", "/schedules", "予約 record に間隔", 400, body=dict(base, snapshot_interval_secs=10))
        self.call("POST", "/schedules", "予約 タイムゾーンが無い", 400, body=dict(base, recurrence={"freq": "daily", "timezone": "Mars/Base"}))
        self.call("POST", "/schedules", "予約 freq の値", 400, body=dict(base, recurrence={"freq": "hourly", "timezone": "UTC"}))
        self.call("POST", "/schedules", "予約 action の値", 400, body=dict(base, action="dance"))
        self.call("POST", "/schedules", "予約 未知のフィールド", 400, body=dict(base, foo=1))
        self.call("POST", "/schedules", "予約 stream が無い", 404, body=dict(base, stream="nosuch"))
        self.call("POST", "/schedules", "予約 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/schedules", "予約 snapshot", 201, body=dict(base, action="snapshot", snapshot_interval_secs=10))
        self.call("POST", "/schedules", "予約 live", 201, body=dict(base, action="live", name="live-window"))
        # すぐ始まる予約（実行履歴を作る）
        s, _, soon = self.call("POST", "/schedules", "予約 すぐ始まる", 201,
                               body={"stream": sid, "action": "live", "start_at": self.ts(1), "end_at": self.ts(3)})
        s, _, lst = self.call("GET", "/schedules?limit=2", "予約の一覧", 200)
        if isinstance(lst, dict) and lst.get("next_cursor"):
            self.call("GET", f"/schedules?limit=2&cursor={lst['next_cursor']}", "予約の一覧 次のページ", 200)
        self.call("GET", f"/schedules?stream={sid}&state=pending", "予約の一覧 stream で絞る", 200)
        self.call("GET", "/schedules?x=1", "予約の一覧 知らないクエリ", 400)
        self.call("GET", f"/schedules/{scid}", "予約の取得", 200)
        self.call("GET", "/schedules/nosuch", "予約 無い", 404)
        self.call("GET", f"/schedules/{scid}/runs", "予約の実行履歴（空）", 200)
        self.call("GET", "/schedules/nosuch/runs", "予約の実行履歴 無い", 404)
        self.call("PATCH", f"/schedules/{scid}", "予約 PATCH If-Match 無し", 428, body={"name": "b"})
        self.call("PATCH", f"/schedules/{scid}", "予約 PATCH 古い", 412, headers={"If-Match": '"5"'}, body={"name": "b"})
        self.call("PATCH", f"/schedules/{scid}", "予約 PATCH state の値", 400, headers={"If-Match": '"1"'}, body={"state": "running"})
        self.call("PATCH", f"/schedules/{scid}", "予約 PATCH 未知のフィールド", 400, headers={"If-Match": '"1"'}, body={"action": "live"})
        self.call("PATCH", f"/schedules/{scid}", "予約 PATCH Content-Type 違い", 415, headers={"If-Match": '"1"', "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/schedules/nosuch", "予約 PATCH 無い", 404, headers={"If-Match": '"1"'}, body={})
        s, h, p = self.call("PATCH", f"/schedules/{scid}", "予約 PATCH", 200, headers={"If-Match": '"1"'},
                            body={"name": "b", "end_at": self.ts(5400)})
        s, h, p = self.call("PATCH", f"/schedules/{scid}", "予約 取り消し", 200, headers={"If-Match": h.get("etag", '"2"')},
                            body={"state": "canceled"})
        check(isinstance(p, dict) and p.get("state") == "canceled", "取り消しで state=canceled")
        # 実行履歴
        for _ in range(60):
            s, _, r = self.raw("GET", f"/api/v1/schedules/{soon['id']}/runs")
            if s == 200 and json.loads(r)["items"]:
                break
            time.sleep(0.1)
        s, _, runs = self.call("GET", f"/schedules/{soon['id']}/runs", "予約の実行履歴", 200)
        check(isinstance(runs, dict) and len(runs.get("items", [])) >= 1, "すぐ始まる予約に実行履歴がある", f"{runs}")
        s, _, g = self.call("GET", f"/schedules/{soon['id']}", "実行中の予約", 200)
        if isinstance(g, dict) and g.get("state") == "running":
            self.call("PATCH", f"/schedules/{soon['id']}", "実行中の予約の start_at は変えられない", 409,
                      headers={"If-Match": f'"{g["generation"]}"'}, body={"start_at": self.ts(10)})
        self.call("DELETE", f"/schedules/{scid}", "予約の削除 古い If-Match", 412, headers={"If-Match": '"99"'})
        self.call("DELETE", f"/schedules/{scid}", "予約の削除", 204)
        self.call("DELETE", f"/schedules/{scid}", "予約の削除 2 回目", 404)

    # ------------------------------------------------------------ 字幕・入力・出力・秘密値・パイプライン

    def mk_stream(self, name, **kw):
        s, _, b = self.raw("POST", "/api/v1/streams", body=dict(name=name, **kw))
        if s != 201:
            bad(f"準備: stream {name} を作れない（{s} {b[:200]!r}）")
            return None
        return json.loads(b)["id"]

    def captions(self):
        cap = {"asr_provider": "whisper_cpp", "asr_endpoint": "http://127.0.0.1:9/inference", "language": "ja"}
        s, h, st = self.call("POST", "/streams", "stream 字幕つきの作成", 201, body={"name": "cap", "desired_state": "stopped", "captions": cap})
        if s != 201:
            return
        cid = st["id"]
        check(st.get("captions", {}).get("asr_provider") == "whisper_cpp" and st["captions"].get("targets") == [],
              "captions が既定値つきで返る", f"{st.get('captions')}")
        self.call("POST", "/streams", "captions asr_provider の値", 400, body={"name": "cap2", "captions": dict(cap, asr_provider="x")})
        self.call("POST", "/streams", "captions asr_endpoint の形", 400, body={"name": "cap2", "captions": dict(cap, asr_endpoint="nope")})
        self.call("POST", "/streams", "captions 翻訳に endpoint が無い", 400, body={"name": "cap2", "captions": dict(cap, translate_provider="deepl")})
        self.call("POST", "/streams", "captions 言語の形", 400, body={"name": "cap2", "captions": dict(cap, targets=["english-us"])})
        self.call("POST", "/streams", "captions 未知のフィールド", 400, body={"name": "cap2", "captions": dict(cap, foo=1)})
        self.call("POST", "/streams", "captions 必須が無い", 400, body={"name": "cap2", "captions": {"asr_provider": "openai"}})
        full = dict(cap, asr_provider="openai", asr_endpoint="https://api.example/v1", asr_model="whisper-1",
                    translate_provider="anthropic", translate_endpoint="https://api.example/tr", translate_model="m",
                    asr_credential_ref="secret:cap-key", targets=["en", "zh-Hant"])
        s, _, ck = self.call("POST", "/secrets", "秘密値の登録（字幕用）", 201, body={"name": "cap-key", "value": "k"})
        self.call("PATCH", f"/streams/{cid}", "PATCH captions 無い秘密値", 400, headers={"If-Match": '"1"'},
                  body={"captions": dict(full, asr_credential_ref="secret:nosuch")})
        self.call("POST", "/streams", "captions credential_ref の形", 400, body={"name": "cap2", "captions": dict(cap, asr_credential_ref="cap-key")})
        s, h, p = self.call("PATCH", f"/streams/{cid}", "PATCH captions の置き換え", 200, headers={"If-Match": '"1"'}, body={"captions": full})
        check(isinstance(p, dict) and (p.get("captions") or {}).get("targets") == ["en", "zh-Hant"], "PATCH で captions が置き換わる")
        self.call("PATCH", f"/streams/{cid}", "PATCH captions 不正", 400, headers={"If-Match": h.get("etag", '"2"')},
                  body={"captions": dict(full, translate_provider="babelfish")})
        if isinstance(ck, dict) and ck.get("id"):
            self.call("DELETE", f"/secrets/{ck['id']}", "字幕が使っている秘密値は消せない", 409)
        s, h, p = self.call("PATCH", f"/streams/{cid}", "PATCH captions: null", 200, headers={"If-Match": h.get("etag", '"2"')}, body={"captions": None})
        check(isinstance(p, dict) and p.get("captions") is None, "captions: null で字幕を消す")
        if isinstance(ck, dict) and ck.get("id"):
            self.call("DELETE", f"/secrets/{ck['id']}", "字幕用の秘密値の削除", 204)
        self.raw("DELETE", f"/api/v1/streams/{cid}")

    def io(self, ro):
        sid = self.mk_stream("io")
        if not sid:
            return
        secret_value = "s3cr3t-VALUE-for-openapi-check"
        # 秘密値
        s, h, sec = self.call("POST", "/secrets", "秘密値の登録", 201, body={"name": "pull-key", "value": secret_value})
        check(s == 201 and h.get("location") == f"/api/v1/secrets/{sec['id']}" and sec.get("ref") == "secret:pull-key"
              and secret_value not in json.dumps(sec), "秘密値の Location・ref、値は返らない")
        sec_id = sec["id"] if s == 201 else "x"
        s, h, s2 = self.call("POST", "/secrets", "秘密値（消す用）", 201, headers={"Idempotency-Key": "sec-1"}, body={"name": "tmp-key", "value": "v"})
        s, h, s3 = self.call("POST", "/secrets", "秘密値 Idempotency 再送", 201, headers={"Idempotency-Key": "sec-1"}, body={"name": "tmp-key", "value": "v"})
        check(h.get("idempotency-replayed") == "true" and s3.get("id") == s2.get("id"), "秘密値の再送は同じ id")
        self.call("POST", "/secrets", "秘密値 名前の重複", 409, body={"name": "pull-key", "value": "x"})
        self.call("POST", "/secrets", "秘密値 名前の形", 400, body={"name": ".x", "value": "x"})
        self.call("POST", "/secrets", "秘密値 値が空", 400, body={"name": "e", "value": ""})
        self.call("POST", "/secrets", "秘密値 値が長すぎ", 400, body={"name": "e", "value": "x" * 8193})
        self.call("POST", "/secrets", "秘密値 未知のフィールド", 400, body={"name": "e", "value": "x", "x": 1})
        self.call("POST", "/secrets", "秘密値 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/secrets", "秘密値 stream:read では登録できない", 403, token=ro, body={"name": "e", "value": "x"})
        s, _, lst = self.call("GET", "/secrets", "秘密値の一覧", 200)
        check(isinstance(lst, dict) and secret_value not in json.dumps(lst) and len(lst.get("items", [])) >= 2, "一覧に値は無い")
        self.call("GET", "/secrets?limit=1", "秘密値の一覧 クエリ（読まない）", 200)
        self.call("GET", "/secrets", "秘密値の一覧 stream:read", 403, token=ro)
        self.call("DELETE", f"/secrets/{s2['id']}", "秘密値の削除", 204)
        self.call("DELETE", f"/secrets/{s2['id']}", "秘密値の削除 2 回目", 404)
        self.call("DELETE", "/secrets/nosuch", "秘密値の削除 無い", 404)
        self.call("DELETE", f"/secrets/{sec_id}", "秘密値の削除 stream:read", 403, token=ro)

        # 入力
        body = {"protocol": "rtmp_pull", "url": "rtmp://127.0.0.1:1/live/x", "credential_ref": "secret:pull-key",
                "options": {"note": "a"}}
        s, h, inp = self.call("POST", f"/streams/{sid}/inputs", "入力の作成 rtmp_pull", 201, body=body)
        iid = inp["id"] if s == 201 else "x"
        check(s == 201 and h.get("location") == f"/api/v1/inputs/{iid}" and h.get("etag") == '"1"', "入力の Location・ETag")
        self.call("POST", f"/streams/{sid}/inputs", "入力 知らない protocol", 400, body=dict(body, protocol="ftp_pull"))
        self.call("POST", f"/streams/{sid}/inputs", "入力 URL に資格情報", 400, body=dict(body, url="rtmp://user:pass@127.0.0.1/live/x"))
        self.call("POST", f"/streams/{sid}/inputs", "入力 スキーム違い", 400, body=dict(body, url="http://127.0.0.1/live"))
        self.call("POST", f"/streams/{sid}/inputs", "入力 ホストが無い", 400, body={"protocol": "rtsp_pull", "url": "rtsp:///x"})
        self.call("POST", f"/streams/{sid}/inputs", "入力 無い秘密値", 400, body=dict(body, credential_ref="secret:nosuch"))
        self.call("POST", f"/streams/{sid}/inputs", "入力 credential_ref の形", 400, body=dict(body, credential_ref="pull-key"))
        self.call("POST", f"/streams/{sid}/inputs", "入力 options が多すぎ", 400, body=dict(body, options={f"k{i}": "v" for i in range(17)}))
        self.call("POST", f"/streams/{sid}/inputs", "入力 未知のフィールド", 400, body=dict(body, x=1))
        self.call("POST", f"/streams/{sid}/inputs", "入力 必須が無い", 400, body={"protocol": "rtmp_pull"})
        self.call("POST", f"/streams/{sid}/inputs", "入力 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/streams/nosuch/inputs", "入力 stream が無い", 404, body=body)
        self.call("POST", f"/streams/{sid}/inputs", "入力 stream:read では作れない", 403, token=ro, body=body)
        # SSRF: リンクローカルの宛先は接続時に拒否する
        s, _, ss = self.call("POST", f"/streams/{sid}/inputs", "入力の作成 hls_pull（169.254.169.254）", 201,
                             body={"protocol": "hls_pull", "url": "http://169.254.169.254/x.m3u8"})
        ssrf = ss["id"] if s == 201 else "x"
        got = None
        for _ in range(100):
            st, _, b = self.raw("GET", f"/api/v1/inputs/{ssrf}")
            j = json.loads(b) if st == 200 else {}
            if (j.get("status") or {}).get("last_error"):
                got = j
                break
            time.sleep(0.1)
        s, h, g = self.call("GET", f"/inputs/{ssrf}", "入力の取得（宛先の拒否）", 200)
        st_ = (g.get("status") or {}) if isinstance(g, dict) else {}
        check(got is not None and "許されていない宛先" in (st_.get("last_error") or "") and st_.get("state") in ("error", "backoff"),
              "SSRF: 169.254.169.254 は接続時に拒否され status に出る", f"{st_}")
        check(st_.get("connects") == 0, "SSRF: 接続していない")
        s, _, l = self.call("GET", f"/streams/{sid}/inputs", "入力の一覧", 200)
        check(isinstance(l, dict) and len(l.get("items", [])) == 2, "入力の一覧に 2 件")
        s, _, l1 = self.call("GET", f"/streams/{sid}/inputs?limit=1", "入力の一覧 limit=1", 200)
        if isinstance(l1, dict) and l1.get("next_cursor"):
            self.call("GET", f"/streams/{sid}/inputs?limit=1&cursor={l1['next_cursor']}", "入力の一覧 次のページ", 200)
        self.call("GET", f"/streams/{sid}/inputs", "入力の一覧 stream:read", 200, token=ro)
        self.call("GET", f"/streams/{sid}/inputs?x=1", "入力の一覧 知らないクエリ", 400)
        self.call("GET", "/streams/nosuch/inputs", "入力の一覧 stream が無い", 404)
        self.call("GET", "/inputs/nosuch", "入力 無い", 404)
        # 更新
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH If-Match 無し", 428, body={"enabled": False})
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH 古い", 412, headers={"If-Match": '"9"'}, body={"enabled": False})
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH 未知のフィールド", 400, headers={"If-Match": '"1"'}, body={"protocol": "hls_pull"})
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH URL に資格情報", 400, headers={"If-Match": '"1"'}, body={"url": "rtmp://a:b@h/live/x"})
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH Content-Type 違い", 415, headers={"If-Match": '"1"', "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/inputs/nosuch", "入力 PATCH 無い", 404, headers={"If-Match": '"1"'}, body={})
        self.call("PATCH", f"/inputs/{iid}", "入力 PATCH stream:read", 403, token=ro, headers={"If-Match": '"1"'}, body={})
        s, h, p = self.call("PATCH", f"/inputs/{iid}", "入力 PATCH", 200, headers={"If-Match": '"1"'},
                            body={"enabled": False, "url": "rtmps://127.0.0.1:1/live/y", "options": {}})
        check(isinstance(p, dict) and p.get("generation") == 2 and h.get("etag") == '"2"' and p.get("enabled") is False,
              "入力 PATCH で generation が進む")
        for _ in range(50):
            st, _, b = self.raw("GET", f"/api/v1/inputs/{iid}")
            if st == 200 and json.loads(b).get("status") is None:
                break
            time.sleep(0.1)
        s, _, g = self.call("GET", f"/inputs/{iid}", "無効にした入力", 200)
        check(isinstance(g, dict) and g.get("status") is None, "無効にした入力の status は null", f"{g}")
        # 使っている秘密値は消せない
        self.call("DELETE", f"/secrets/{sec_id}", "使っている秘密値は消せない", 409)
        s, h, p = self.call("PATCH", f"/inputs/{iid}", "入力 PATCH credential_ref: null", 200, headers={"If-Match": '"2"'}, body={"credential_ref": None})
        check(isinstance(p, dict) and p.get("credential_ref") is None, "credential_ref: null で参照を外す")
        # 上限（stream ごとに 4 つ）
        extra = []
        self.call("POST", f"/streams/{sid}/inputs", "入力 device の url の形", 400, body={"protocol": "device", "url": "v4l2:/dev/video0"})
        for i, b in enumerate([{"protocol": "srt_pull", "url": "srt://127.0.0.1:1", "enabled": False},
                               {"protocol": "device", "url": "device:synthetic-0", "enabled": False, "options": {"width": "640"}}]):
            s, _, e = self.call("POST", f"/streams/{sid}/inputs", f"入力 {i + 3} つ目（{b['protocol']}）", 201, body=b)
            if s == 201:
                extra.append(e["id"])
        self.call("POST", f"/streams/{sid}/inputs", "入力 5 つ目は 403", 403, body={"protocol": "srt_pull", "url": "srt://127.0.0.1:9", "enabled": False})
        self.call("DELETE", f"/inputs/{iid}", "入力 削除 stream:read", 403, token=ro)

        # 出力
        ob = {"protocol": "rtmp_push", "url": "rtmp://127.0.0.1:1/live/key", "credential_ref": "secret:pull-key"}
        s, h, out = self.call("POST", f"/streams/{sid}/outputs", "出力の作成 rtmp_push", 201, body=ob)
        oid = out["id"] if s == 201 else "x"
        check(s == 201 and h.get("location") == f"/api/v1/outputs/{oid}" and h.get("etag") == '"1"', "出力の Location・ETag")
        s, _, o2 = self.call("POST", f"/streams/{sid}/outputs", "出力の作成 srt_push（無効）", 201,
                             body={"protocol": "srt_push", "url": "srt://127.0.0.1:1", "enabled": False})
        self.call("POST", f"/streams/{sid}/outputs", "出力 知らない protocol", 400, body=dict(ob, protocol="ftp_push"))
        self.call("POST", f"/streams/{sid}/outputs", "出力 URL に資格情報", 400, body=dict(ob, url="rtmp://u:p@127.0.0.1/live/k"))
        self.call("POST", f"/streams/{sid}/outputs", "出力 url が無い", 400, body={"protocol": "rtmp_push"})
        self.call("POST", f"/streams/{sid}/outputs", "出力 paas に provider が無い", 400, body={"protocol": "paas", "credential_ref": "secret:pull-key"})
        self.call("POST", f"/streams/{sid}/outputs", "出力 paas 知らない provider", 400,
                  body={"protocol": "paas", "provider": "acme", "credential_ref": "secret:pull-key"})
        self.call("POST", f"/streams/{sid}/outputs", "出力 paas に credential_ref が無い", 400, body={"protocol": "paas", "provider": "aws_ivs"})
        self.call("POST", f"/streams/{sid}/outputs", "出力 無い秘密値", 400, body=dict(ob, credential_ref="secret:nosuch"))
        self.call("POST", f"/streams/{sid}/outputs", "出力 未知のフィールド", 400, body=dict(ob, x=1))
        self.call("POST", f"/streams/{sid}/outputs", "出力 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/streams/nosuch/outputs", "出力 stream が無い", 404, body=ob)
        self.call("POST", f"/streams/{sid}/outputs", "出力 stream:read では作れない", 403, token=ro, body=ob)
        # 配信していないので waiting_input になる
        for _ in range(50):
            st, _, b = self.raw("GET", f"/api/v1/outputs/{oid}")
            if st == 200 and (json.loads(b).get("status") or {}).get("state") == "waiting_input":
                break
            time.sleep(0.1)
        s, h, g = self.call("GET", f"/outputs/{oid}", "出力の取得", 200)
        check(isinstance(g, dict) and (g.get("status") or {}).get("state") == "waiting_input", "配信前の出力は waiting_input", f"{g.get('status') if isinstance(g, dict) else g}")
        self.call("GET", "/outputs/nosuch", "出力 無い", 404)
        s, _, l = self.call("GET", f"/streams/{sid}/outputs", "出力の一覧", 200)
        check(isinstance(l, dict) and len(l.get("items", [])) == 2, "出力の一覧に 2 件")
        self.call("GET", f"/streams/{sid}/outputs?limit=1", "出力の一覧 limit=1", 200)
        self.call("GET", f"/streams/{sid}/outputs?x=1", "出力の一覧 知らないクエリ", 400)
        self.call("GET", "/streams/nosuch/outputs", "出力の一覧 stream が無い", 404)
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH If-Match 無し", 428, body={"enabled": False})
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH 古い", 412, headers={"If-Match": '"9"'}, body={"enabled": False})
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH 未知のフィールド", 400, headers={"If-Match": '"1"'}, body={"provider": "aws_ivs"})
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH スキーム違い", 400, headers={"If-Match": '"1"'}, body={"url": "srt://127.0.0.1:1"})
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH Content-Type 違い", 415, headers={"If-Match": '"1"', "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/outputs/nosuch", "出力 PATCH 無い", 404, headers={"If-Match": '"1"'}, body={})
        self.call("PATCH", f"/outputs/{oid}", "出力 PATCH stream:read", 403, token=ro, headers={"If-Match": '"1"'}, body={})
        s, h, p = self.call("PATCH", f"/outputs/{oid}", "出力 PATCH", 200, headers={"If-Match": '"1"'},
                            body={"enabled": False, "credential_ref": None, "url": "rtmp://127.0.0.1:2/live/k2"})
        check(isinstance(p, dict) and p.get("generation") == 2 and p.get("credential_ref") is None,
              "出力 PATCH で generation が進む")
        self.call("DELETE", f"/outputs/{oid}", "出力 削除 stream:read", 403, token=ro)
        self.call("DELETE", f"/outputs/{oid}", "出力の削除", 204)
        self.call("DELETE", f"/outputs/{oid}", "出力の削除 2 回目", 404)
        if isinstance(o2, dict) and o2.get("id"):
            self.call("DELETE", f"/outputs/{o2['id']}", "出力の削除 srt", 204)
        for x in [iid, ssrf] + extra:
            self.call("DELETE", f"/inputs/{x}", "入力の削除", 204)
        self.call("DELETE", f"/inputs/{iid}", "入力の削除 2 回目", 404)
        self.call("DELETE", f"/secrets/{sec_id}", "参照が無くなった秘密値の削除", 204)
        self.raw("DELETE", f"/api/v1/streams/{sid}")

    def graph(self, src, dst, vcodec="vp9", enc="av1"):
        return {"version": 1, "options": {"status_interval_ms": 200}, "nodes": [
            {"type": "source", "id": "cam", "stream": src,
             "video": {"codec": vcodec, "width": 640, "height": 360, "fps": 30},
             "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2}},
            {"type": "decode", "id": "d", "input": "cam.video"},
            {"type": "scale", "id": "s", "input": "d", "width": 320, "height": 180, "filter": "bilinear"},
            {"type": "encode", "id": "e", "input": "s", "codec": enc, "bitrate_kbps": 500},
            {"type": "sink", "id": "o", "inputs": ["e", "cam.audio"], "stream": dst},
        ]}

    def pipelines(self, ro):
        rf = {"codec_profile": "royalty_free", "hls": {"container": "fmp4"}}
        src = self.mk_stream("pl-src", **rf)
        dst = self.mk_stream("pl-dst", **rf)
        anyp = self.mk_stream("pl-any")
        if not (src and dst and anyp):
            return
        g = self.graph(src, dst)
        s, h, p = self.call("POST", "/pipelines", "パイプラインの作成（VP9 → AV1）", 201, body={"name": "pl", "graph": g})
        if s != 201:
            return
        pid = p["id"]
        check(h.get("location") == f"/api/v1/pipelines/{pid}" and h.get("etag") == '"1"' and p.get("observed_state") == "stopped"
              and p.get("plan") is None and p.get("meters") is None, "パイプラインの Location・ETag、既定は stopped")
        self.call("POST", "/pipelines", "パイプライン 名前の重複", 409, body={"name": "pl", "graph": g})
        bad_g = json.loads(json.dumps(g))
        bad_g["nodes"][0]["stream"] = "nosuch"
        s, _, e = self.call("POST", "/pipelines", "パイプライン 知らない stream", 400, body={"name": "pl2", "graph": bad_g})
        check(isinstance(e, dict) and e.get("code") == "INVALID_CONFIG", "知らない stream は INVALID_CONFIG")
        s, _, e = self.call("POST", "/pipelines", "パイプライン 知らない参照", 400,
                            body={"name": "pl2", "graph": {"version": 1, "nodes": [{"type": "decode", "id": "d", "input": "x"}]}})
        check(isinstance(e, dict) and any(i.get("code") == "UNKNOWN_REFERENCE" for i in (e.get("details") or {}).get("issues", [])),
              "details.issues に UNKNOWN_REFERENCE", f"{e}")
        if isinstance(e, dict):
            errs = [x for i in (e.get("details") or {}).get("issues", []) for x in self.v.errors(i, {"$ref": "#/components/schemas/PipelineIssue"})]
            check(not errs, "details.issues が PipelineIssue に合う", "; ".join(errs))
        self.call("POST", "/pipelines", "パイプライン royalty_free に H.264", 400, body={"name": "pl2", "graph": self.graph(src, dst, "h264", "h264")})
        self.call("POST", "/pipelines", "パイプライン グラフの版", 400, body={"name": "pl2", "graph": dict(g, version=2)})
        self.call("POST", "/pipelines", "パイプライン 知らない部品", 400, body={"name": "pl2", "graph": {"version": 1, "nodes": [{"type": "magic", "id": "m"}]}})
        self.call("POST", "/pipelines", "パイプライン 名前の形", 400, body={"name": ".pl", "graph": g})
        self.call("POST", "/pipelines", "パイプライン desired_state の値", 400, body={"name": "pl2", "graph": g, "desired_state": "paused"})
        self.call("POST", "/pipelines", "パイプライン 未知のフィールド", 400, body={"name": "pl2", "graph": g, "x": 1})
        self.call("POST", "/pipelines", "パイプライン Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/pipelines", "パイプライン stream:read では作れない", 403, token=ro, body={"name": "pl2", "graph": g})
        # H.264 は装置（ハードウェア）が無ければ 503
        self.call("POST", "/pipelines", "パイプライン H.264（装置が無ければ 503）", [201, 503],
                  body={"name": "pl-h264", "graph": self.graph(anyp, anyp, "h264", "h264")})
        # 一覧・取得
        s, _, l = self.call("GET", "/pipelines", "パイプラインの一覧", 200)
        s, _, l1 = self.call("GET", "/pipelines?limit=1", "パイプラインの一覧 limit=1", 200)
        if isinstance(l1, dict) and l1.get("next_cursor"):
            self.call("GET", f"/pipelines?limit=1&cursor={l1['next_cursor']}", "パイプラインの一覧 次のページ", 200)
        self.call("GET", "/pipelines", "パイプラインの一覧 stream:read", 200, token=ro)
        self.call("GET", "/pipelines?x=1", "パイプラインの一覧 知らないクエリ", 400)
        self.call("GET", f"/pipelines/{pid}", "パイプラインの取得", 200)
        self.call("GET", "/pipelines/nosuch", "パイプライン 無い", 404)
        # 検証
        s, _, v = self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証（保存したグラフ）", 200)
        check(isinstance(v, dict) and (v.get("plan") or {}).get("order") == ["cam", "d", "s", "e", "o"], "検証の計画に実行の順")
        self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証（本文のグラフ）", 200, body=self.graph(src, dst, "av1", "vp9"))
        self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証 知らない stream", 400, body=bad_g)
        self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証 H.264（装置が無ければ 503）", [200, 503], body=self.graph(anyp, anyp, "h264", "h264"))
        self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証 stream:read", 200, token=ro)
        self.call("POST", f"/pipelines/{pid}/validate", "パイプラインの検証 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", "/pipelines/nosuch/validate", "パイプラインの検証 無い", 404)
        # 止まっているパイプラインへの操作は 409
        self.call("POST", f"/pipelines/{pid}/control", "操作 止まっているパイプライン", 409, body={"op": "request_keyframe"})
        self.call("POST", f"/pipelines/{pid}/control", "操作 世代の操作は 400", 400, body={"op": "commit", "generation": 1})
        self.call("POST", f"/pipelines/{pid}/control", "操作 知らない op", 400, body={"op": "explode"})
        self.call("POST", f"/pipelines/{pid}/control", "操作 未知のフィールド", 400, body={"op": "set_master", "node": "m", "gain_db": 0, "x": 1})
        self.call("POST", f"/pipelines/{pid}/control", "操作 Content-Type 違い", 415, body=b"{}", headers={"Content-Type": "text/plain"})
        self.call("POST", f"/pipelines/{pid}/control", "操作 stream:read", 403, token=ro, body={"op": "request_keyframe"})
        self.call("POST", "/pipelines/nosuch/control", "操作 無い", 404, body={"op": "request_keyframe"})
        # 更新・開始
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH If-Match 無し", 428, body={"desired_state": "running"})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH 古い", 412, headers={"If-Match": '"9"'}, body={"desired_state": "running"})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH desired_state の値", 400, headers={"If-Match": '"1"'}, body={"desired_state": "x"})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH 不正なグラフ", 400, headers={"If-Match": '"1"'}, body={"graph": bad_g})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH 未知のフィールド", 400, headers={"If-Match": '"1"'}, body={"x": 1})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH Content-Type 違い", 415, headers={"If-Match": '"1"', "Content-Type": "text/plain"}, body=b"{}")
        self.call("PATCH", "/pipelines/nosuch", "パイプライン PATCH 無い", 404, headers={"If-Match": '"1"'}, body={})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH stream:read", 403, token=ro, headers={"If-Match": '"1"'}, body={})
        s, h, p = self.call("PATCH", f"/pipelines/{pid}", "パイプラインの開始", 200, headers={"If-Match": '"1"'},
                            body={"desired_state": "running", "graph": self.graph(src, dst, "vp9", "vp9")})
        check(isinstance(p, dict) and p.get("generation") == 2 and h.get("etag") == '"2"' and p.get("desired_state") == "running",
              "PATCH で generation が進み desired_state=running")
        running = None
        for _ in range(100):
            st, _, b = self.raw("GET", f"/api/v1/pipelines/{pid}")
            j = json.loads(b) if st == 200 else {}
            if j.get("observed_state") == "running" and j.get("status"):
                running = j
                break
            time.sleep(0.1)
        s, _, g2 = self.call("GET", f"/pipelines/{pid}", "動いているパイプライン", 200)
        check(running is not None, "パイプラインが running になり status が出る", f"{g2.get('observed_state') if isinstance(g2, dict) else g2} {g2.get('last_error') if isinstance(g2, dict) else ''}")
        if running is not None:
            check(isinstance(g2.get("plan"), dict) and isinstance(g2.get("meters"), dict), "動いていれば plan と meters がある")
            for _ in range(30):
                st, _, b = self.raw("GET", f"/api/v1/pipelines/{pid}")
                if (json.loads(b).get("status") or {}).get("event") == "status":
                    break
                time.sleep(0.1)
            s, _, g3 = self.call("GET", f"/pipelines/{pid}", "動いているパイプライン（status）", 200)
            ev = ((g3 if isinstance(g3, dict) else {}).get("status") or {})
            check(ev.get("event") == "status" and [x.get("id") for x in ev.get("sources", [])] == ["cam"],
                  "status に入力の状態がある", f"{ev.get('event')}")
            self.call("POST", f"/pipelines/{pid}/control", "操作 request_keyframe", 200, body={"op": "request_keyframe"})
            self.call("POST", f"/pipelines/{pid}/control", "操作 request_keyframe（部品の指定）", 200, body={"op": "request_keyframe", "node": "e"})
            self.call("POST", f"/pipelines/{pid}/control", "操作 知らない部品は 400", 400, body={"op": "set_master", "node": "nosuch", "gain_db": 0})
        s, _, p2 = self.call("POST", "/pipelines", "パイプライン 2 本目", 201, body={"name": "pl-other", "graph": g})
        self.call("PATCH", f"/pipelines/{pid}", "パイプライン PATCH 名前の重複", 409, headers={"If-Match": '"2"'}, body={"name": "pl-other"})
        s, h, p = self.call("PATCH", f"/pipelines/{pid}", "パイプラインの停止", 200, headers={"If-Match": '"2"'}, body={"desired_state": "stopped", "name": "pl-renamed"})
        check(isinstance(p, dict) and p.get("name") == "pl-renamed" and p.get("desired_state") == "stopped", "停止と名前の変更")
        self.call("DELETE", f"/pipelines/{pid}", "パイプライン 削除 stream:read", 403, token=ro)
        self.call("DELETE", f"/pipelines/{pid}", "パイプラインの削除", 204)
        self.call("DELETE", f"/pipelines/{pid}", "パイプラインの削除 2 回目", 404)
        self.call("GET", f"/pipelines/{pid}", "削除したパイプライン", 404)
        s, _, l = self.raw("GET", "/api/v1/pipelines")
        for x in json.loads(l).get("items", []):
            self.raw("DELETE", f"/api/v1/pipelines/{x['id']}")
        for x in (src, dst, anyp):
            self.raw("DELETE", f"/api/v1/streams/{x}")

    def events(self, sub_tok):
        # 認証なし・権限なし
        for label, tok, want in [("SSE 認証なし", None, 401), ("SSE stream:read が無い", sub_tok, 403)]:
            h = {} if tok is None else {"Authorization": f"Bearer {tok}"}
            s, hd, d = self.raw("GET", "/api/v1/events", headers=h, auth=False)
            self.verify("GET", "/events", s, hd, d, label, self.api, api=True)
            check(s == want, f"{label}: 状態 {s}")
        # 200: resync と、変更のイベント
        sock = socket.create_connection((self.host, self.port), timeout=10)
        sock.sendall((f"GET /api/v1/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {self.token}\r\n"
                      "Last-Event-ID: 1\r\nAccept: text/event-stream\r\n\r\n").encode())
        f = sock.makefile("rb")
        status_line = f.readline().decode()
        hdrs = {}
        while True:
            line = f.readline().decode().rstrip("\r\n")
            if not line:
                break
            k, _, v = line.partition(":")
            hdrs[k.strip().lower()] = v.strip()
        status = int(status_line.split()[1])
        self.verify("GET", "/events", status, hdrs, b"", "SSE 購読", self.api, api=True)
        # 変更を起こす
        s, _, st = self.raw("POST", "/api/v1/streams", body={"name": "sse-probe"})
        probe = json.loads(st)["id"]
        self.raw("DELETE", f"/api/v1/streams/{probe}")
        events = []
        cur = {}
        deadline = time.time() + 10
        while time.time() < deadline and len([e for e in events if e.get("event") != "resync"]) < 2:
            line = f.readline().decode().rstrip("\r\n")
            if line == "":
                if cur:
                    events.append(cur)
                cur = {}
                continue
            if line.startswith(":"):
                continue
            k, _, v = line.partition(":")
            v = v[1:] if v.startswith(" ") else v
            cur[k] = cur.get(k, "") + v if k == "data" else v
        sock.close()
        check(events and events[0].get("event") == "resync", "SSE Last-Event-ID が古いと最初に resync", f"{events[:1]}")
        sch = self.spec["paths"]["/events"]["get"]["responses"]["200"]["content"]["text/event-stream"]["x-senca-event-data"]
        types = []
        for e in events:
            if e.get("event") == "resync":
                continue
            try:
                d = json.loads(e.get("data", ""))
            except ValueError:
                bad(f"SSE data が JSON でない: {e}")
                continue
            errs = self.v.errors(d, sch)
            check(not errs and e.get("event") == d.get("type") and e.get("id", "").isdigit(),
                  f"SSE イベント {e.get('event')} が文書どおり", "; ".join(errs))
            types.append(d.get("type"))
        check("stream.created" in types and "stream.deleted" in types, "SSE に stream.created / stream.deleted が流れる", f"{types}")

    def methods(self):
        self.call("PUT", "/streams", "PUT /streams は 405", 405, body={})
        self.call("DELETE", "/capabilities", "DELETE /capabilities は 405", 405)
        self.call("POST", "/events", "POST /events は 405", 405)
        self.call("GET", "/tokens/abc", "GET /tokens/{id}（無い ID）は 404", 404)
        self.call("GET", "/nosuch", "知らないパスは 404", 404)
        self.call("GET", "/streams//x", "空の区切りは 404", 404)
        self.call("GET", "/", "/api/v1/ は 404", 404)

    def cleanup(self, sid, other, ro):
        self.call("DELETE", f"/streams/{sid}", "stream 削除 古い If-Match", 412, headers={"If-Match": '"99"'})
        self.call("DELETE", f"/streams/{sid}", "stream 削除 stream:read", 403, token=ro)
        self.call("DELETE", f"/streams/{sid}", "stream 削除（仕様は 202、実装は 204）", 204)
        self.call("GET", f"/streams/{sid}", "削除した stream", 404)
        self.call("DELETE", f"/streams/{sid}", "stream 削除 2 回目", 404)
        s, _, sch = self.call("GET", f"/schedules?stream={sid}", "stream と一緒に予約も消える", 200)
        check(isinstance(sch, dict) and sch.get("items") == [], "stream の削除で予約が消える")
        self.call("DELETE", f"/streams/{other}", "stream 削除 other", 204)
        self.call("DELETE", f"/streams/{self.idem}", "stream 削除 idem", 204)
        self.call("DELETE", f"/tokens/{self.ro_id}", "token 削除 ro", 204)

    def coverage(self):
        missing = []
        for p, item in self.spec["paths"].items():
            for m in METHODS:
                if m in item and (m.upper(), p) not in self.exercised:
                    missing.append(f"{m.upper()} {p}")
        n = sum(1 for p, item in self.spec["paths"].items() for m in METHODS if m in item)
        if missing:
            bad(f"coverage: 試していない操作 {missing}")
        else:
            ok(f"coverage: 文書の {n} 操作をすべて実地で試した")
        documented = sum(len(op["responses"]) for item in self.spec["paths"].values() for m in METHODS if m in item for op in [item[m]])
        seen = sum(len(v) for v in self.exercised.values())
        print(f"== 状態コード: 文書 {documented} 個のうち {seen} 個を実際に観測")


def main():
    if len(sys.argv) >= 4 and sys.argv[1] == "routes":
        cmd_routes(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 8 and sys.argv[1] == "live":
        Live(load_spec(sys.argv[2]), *sys.argv[3:8]).run()
    else:
        print(__doc__)
        sys.exit(2)
    print(f"== {PASS} passed, {FAIL} failed")
    sys.exit(0 if FAIL == 0 else 1)


if __name__ == "__main__":
    main()
