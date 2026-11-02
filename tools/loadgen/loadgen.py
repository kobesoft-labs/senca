#!/usr/bin/env python3
"""SENCA の負荷・長時間試験の道具（REQ-010 R10-6、AC-12・AC-21）。標準ライブラリだけを使う。

サブコマンド:
  viewers   HLS の視聴者 N 人（署名付き URL を何本でも）。playlist を更新し、新しい segment を取りに行く。
            segment の取得時間（要求から最後のバイトまで）の p50/p95/p99、誤り、bytes/s を出す。
  scenario  ノードの API で stream を M 本作り、M 本の RTMP 送信（FFmpeg -re -c copy）を始め、
            署名付きの再生 URL に N 人の視聴者を振り分ける。--churn で視聴者・送り手の接続・切断を繰り返す。
  sample    プロセスの RSS・CPU・fd・スレッド・socket を一定間隔で CSV に書く（soak.sh が使う）。
  analyze   sample の CSV から warm-up 後の RSS の増分などを判定する（AC-21）。

例:
  loadgen.py scenario --api http://127.0.0.1:8080/api/v1 --token-file DATA/control/bootstrap-admin-token \\
      --rtmp rtmp://127.0.0.1:1935 --publishers 4 --viewers 200 --duration 60 --out result.json

注意: 開発機の小規模な測定のための道具。AC-12（1000 視聴・4.2Gbps）の判定は仕様 13.1 の基準環境で、
送り手・視聴者を別の機械から出して行う（この道具は --procs で複数プロセスに分けられる）。
"""

import argparse
import asyncio
import json
import math
import multiprocessing
import os
import random
import signal
import statistics
import subprocess
import sys
import time
import urllib.parse
import urllib.request

# ------------------------------------------------------------------ 小さな HTTP/1.1 client（keep-alive）


class HttpError(Exception):
    pass


class Conn:
    """1 本の keep-alive 接続。相手が閉じたら作り直す。"""

    def __init__(self, host, port):
        self.host, self.port = host, port
        self.r = self.w = None

    async def open(self):
        self.r, self.w = await asyncio.open_connection(self.host, self.port, limit=1 << 20)

    def close(self):
        if self.w:
            self.w.close()
        self.r = self.w = None

    async def get(self, path, timeout):
        """(status, body_len, body or None) を返す。body は playlist のときだけ返す（segment は捨てる）。"""
        for attempt in (0, 1):
            if self.w is None:
                await self.open()
            try:
                return await asyncio.wait_for(self._get(path), timeout)
            except (ConnectionError, asyncio.IncompleteReadError, HttpError) as e:
                self.close()
                if attempt == 1 or not isinstance(e, (ConnectionError, asyncio.IncompleteReadError)):
                    raise
            except asyncio.TimeoutError:
                self.close()
                raise

    async def _get(self, path):
        req = f"GET {path} HTTP/1.1\r\nHost: {self.host}:{self.port}\r\nUser-Agent: senca-loadgen\r\nAccept: */*\r\n\r\n"
        self.w.write(req.encode())
        await self.w.drain()
        line = await self.r.readuntil(b"\r\n")
        parts = line.split(b" ", 2)
        if len(parts) < 2 or not parts[0].startswith(b"HTTP/1."):
            raise HttpError(f"bad status line {line[:40]!r}")
        status = int(parts[1])
        clen = None
        chunked = False
        close = False
        while True:
            h = await self.r.readuntil(b"\r\n")
            if h == b"\r\n":
                break
            k, _, v = h.decode("latin1").partition(":")
            k = k.strip().lower()
            v = v.strip()
            if k == "content-length":
                clen = int(v)
            elif k == "transfer-encoding" and "chunked" in v.lower():
                chunked = True
            elif k == "connection" and v.lower() == "close":
                close = True
        keep = path.split("?", 1)[0].endswith(".m3u8")
        body = bytearray() if keep else None
        n = 0
        if chunked:
            while True:
                sz = int((await self.r.readuntil(b"\r\n")).split(b";")[0], 16)
                if sz == 0:
                    while (await self.r.readuntil(b"\r\n")) != b"\r\n":
                        pass
                    break
                n += await self._read_n(sz, body)
                await self.r.readexactly(2)
        elif clen is not None:
            n = await self._read_n(clen, body)
        else:
            close = True
            while True:
                d = await self.r.read(65536)
                if not d:
                    break
                n += len(d)
                if body is not None:
                    body += d
        if close:
            self.close()
        return status, n, (bytes(body) if body is not None else None)

    async def _read_n(self, n, body):
        left = n
        while left > 0:
            d = await self.r.read(min(left, 262144))
            if not d:
                raise asyncio.IncompleteReadError(b"", left)
            left -= len(d)
            if body is not None:
                body += d
        return n


# ------------------------------------------------------------------ HLS の視聴者


def parse_playlist(text):
    """(種類, 情報)。multivariant なら最初の variant の URI、media なら segment の一覧。"""
    lines = [l.strip() for l in text.splitlines()]
    if any(l.startswith("#EXT-X-STREAM-INF") for l in lines):
        for i, l in enumerate(lines):
            if l.startswith("#EXT-X-STREAM-INF") and i + 1 < len(lines):
                return "multi", lines[i + 1]
        return "multi", None
    seq = 0
    target = 2.0
    segs = []
    init = None
    dur = None
    ended = False
    for l in lines:
        if l.startswith("#EXT-X-MEDIA-SEQUENCE:"):
            seq = int(l.split(":", 1)[1])
        elif l.startswith("#EXT-X-TARGETDURATION:"):
            target = float(l.split(":", 1)[1])
        elif l.startswith("#EXT-X-MAP:"):
            for kv in l.split(":", 1)[1].split(","):
                if kv.startswith("URI="):
                    init = kv[4:].strip('"')
        elif l.startswith("#EXTINF:"):
            dur = float(l.split(":", 1)[1].split(",")[0])
        elif l.startswith("#EXT-X-ENDLIST"):
            ended = True
        elif l and not l.startswith("#"):
            segs.append((seq + len(segs), l, dur or target))
            dur = None
    return "media", {"segments": segs, "target": target, "init": init, "ended": ended}


class Stats:
    def __init__(self):
        self.seg_lat = []
        self.pl_lat = []
        self.bytes = 0
        self.segments = 0
        self.playlists = 0
        self.errors = {}
        self.sessions = 0
        self.late = 0  # segment の取得に再生時間より長くかかった（視聴者が止まる目安）
        self.first_seg = []  # 視聴開始から最初の segment を受け取るまで

    def err(self, kind):
        self.errors[kind] = self.errors.get(kind, 0) + 1

    def merge(self, d):
        self.seg_lat += d["seg_lat"]
        self.pl_lat += d["pl_lat"]
        self.first_seg += d["first_seg"]
        self.bytes += d["bytes"]
        self.segments += d["segments"]
        self.playlists += d["playlists"]
        self.sessions += d["sessions"]
        self.late += d["late"]
        for k, v in d["errors"].items():
            self.errors[k] = self.errors.get(k, 0) + v

    def raw(self):
        return {k: getattr(self, k) for k in
                ("seg_lat", "pl_lat", "first_seg", "bytes", "segments", "playlists", "sessions", "late", "errors")}


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    k = (len(xs) - 1) * p / 100
    f = math.floor(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


async def viewer(url, deadline, stats, churn_mean, timeout, start_delay):
    await asyncio.sleep(start_delay)
    while time.monotonic() < deadline:
        session_end = deadline if not churn_mean else min(deadline, time.monotonic() + random.expovariate(1 / churn_mean))
        stats.sessions += 1
        await viewer_session(url, session_end, stats, timeout)
        if churn_mean and time.monotonic() < deadline:
            await asyncio.sleep(random.uniform(0.1, 1.0))


async def viewer_session(url, session_end, stats, timeout):
    u = urllib.parse.urlsplit(url)
    conn = Conn(u.hostname, u.port or 80)
    base = url
    path = u.path + ("?" + u.query if u.query else "")
    last_seq = None
    t_start = time.monotonic()
    got_first = False
    init_done = False
    try:
        while time.monotonic() < session_end:
            t0 = time.monotonic()
            try:
                status, _, body = await conn.get(path, timeout)
            except Exception as e:
                stats.err(f"playlist:{type(e).__name__}")
                await asyncio.sleep(1)
                continue
            stats.pl_lat.append(time.monotonic() - t0)
            stats.playlists += 1
            if status != 200:
                stats.err(f"playlist:{status}")
                await asyncio.sleep(1)
                continue
            kind, info = parse_playlist(body.decode("utf-8", "replace"))
            if kind == "multi":
                if not info:
                    stats.err("playlist:no-variant")
                    return
                base = urllib.parse.urljoin(base, info)
                bu = urllib.parse.urlsplit(base)
                path = bu.path + ("?" + bu.query if bu.query else "")
                continue
            segs = info["segments"]
            if info["init"] and not init_done:
                iu = urllib.parse.urlsplit(urllib.parse.urljoin(base, info["init"]))
                try:
                    st, n, _ = await conn.get(iu.path + ("?" + iu.query if iu.query else ""), timeout)
                    stats.bytes += n
                    init_done = st == 200
                except Exception as e:
                    stats.err(f"init:{type(e).__name__}")
            if last_seq is None:
                # live の端から 3 本手前で始める（一般的な player と同じ）
                todo = segs[-3:]
            else:
                todo = [s for s in segs if s[0] > last_seq]
            for seq, uri, dur in todo:
                if time.monotonic() >= session_end:
                    break
                su = urllib.parse.urlsplit(urllib.parse.urljoin(base, uri))
                t1 = time.monotonic()
                try:
                    st, n, _ = await conn.get(su.path + ("?" + su.query if su.query else ""), timeout)
                except Exception as e:
                    stats.err(f"segment:{type(e).__name__}")
                    continue
                dt = time.monotonic() - t1
                if st != 200:
                    stats.err(f"segment:{st}")
                    continue
                stats.seg_lat.append(dt)
                stats.segments += 1
                stats.bytes += n
                if dt > dur:
                    stats.late += 1
                if not got_first:
                    stats.first_seg.append(time.monotonic() - t_start)
                    got_first = True
                last_seq = seq
            if info["ended"]:
                return
            # playlist の更新間隔: target duration の半分（RFC 8216 6.3.4 の再読込み）
            await asyncio.sleep(max(0.2, info["target"] / 2))
    finally:
        conn.close()


async def run_viewers(urls, n, duration, ramp, churn_mean, timeout, seed):
    random.seed(seed)
    stats = Stats()
    deadline = time.monotonic() + duration
    tasks = []
    for i in range(n):
        delay = ramp * i / max(1, n)
        tasks.append(asyncio.create_task(viewer(urls[i % len(urls)], deadline, stats, churn_mean, timeout, delay)))
    await asyncio.gather(*tasks, return_exceptions=True)
    return stats


def viewers_proc(args):
    urls, n, duration, ramp, churn, timeout, seed, q = args
    try:
        import resource
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, max(soft, 8192)), hard))
    except Exception:
        pass
    st = asyncio.run(run_viewers(urls, n, duration, ramp, churn, timeout, seed))
    q.put(st.raw())


def run_viewers_multi(urls, n, duration, ramp, churn, timeout, procs):
    procs = max(1, min(procs, n))
    q = multiprocessing.Queue()
    ps = []
    for i in range(procs):
        k = n // procs + (1 if i < n % procs else 0)
        sub = urls[i % len(urls):] + urls[:i % len(urls)]
        p = multiprocessing.Process(target=viewers_proc, args=((sub, k, duration, ramp, churn, timeout, 1000 + i, q),))
        p.start()
        ps.append(p)
    total = Stats()
    for _ in ps:
        total.merge(q.get())
    for p in ps:
        p.join()
    return total


def summarize(st, duration, n):
    ms = lambda x: None if x is None else round(x * 1000, 1)
    return {
        "viewers": n,
        "duration_s": duration,
        "sessions": st.sessions,
        "segments": st.segments,
        "playlists": st.playlists,
        "bytes": st.bytes,
        "throughput_mbps": round(st.bytes * 8 / duration / 1e6, 2),
        "segment_ms": {"p50": ms(pct(st.seg_lat, 50)), "p95": ms(pct(st.seg_lat, 95)),
                       "p99": ms(pct(st.seg_lat, 99)), "max": ms(max(st.seg_lat) if st.seg_lat else None)},
        "playlist_ms": {"p50": ms(pct(st.pl_lat, 50)), "p95": ms(pct(st.pl_lat, 95)), "p99": ms(pct(st.pl_lat, 99))},
        "first_segment_ms": {"p50": ms(pct(st.first_seg, 50)), "p95": ms(pct(st.first_seg, 95))},
        "late_segments": st.late,
        "errors": st.errors,
        "error_count": sum(st.errors.values()),
    }


def print_summary(s, prefix=""):
    print(f"{prefix}視聴者 {s['viewers']}・{s['duration_s']}s・接続 {s['sessions']}: segment {s['segments']} 本、"
          f"{s['throughput_mbps']} Mbit/s、取得 p50/p95/p99 = {s['segment_ms']['p50']}/{s['segment_ms']['p95']}/"
          f"{s['segment_ms']['p99']} ms、playlist p95 {s['playlist_ms']['p95']} ms、遅れ {s['late_segments']}、"
          f"誤り {s['error_count']} {s['errors'] or ''}")


# ------------------------------------------------------------------ 送り手（FFmpeg）


class Publisher:
    def __init__(self, ffmpeg, src, url, log):
        self.ffmpeg, self.src, self.url, self.log = ffmpeg, src, url, log
        self.p = None
        self.starts = 0
        self.failures = 0

    def start(self):
        self.starts += 1
        self.p = subprocess.Popen(
            [self.ffmpeg, "-hide_banner", "-loglevel", "error", "-re", "-stream_loop", "-1", "-i", self.src,
             "-c", "copy", "-f", "flv", self.url],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.log)

    def alive(self):
        return self.p is not None and self.p.poll() is None

    def stop(self):
        if self.p and self.p.poll() is None:
            self.p.send_signal(signal.SIGINT)
            try:
                self.p.wait(3)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        self.p = None


def api(base, token, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read() or b"null")


def make_source(ffmpeg, path, bitrate_k, size, fps, gop_s, seconds):
    if os.path.exists(path):
        return
    subprocess.run([ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
                    "-f", "lavfi", "-i", f"testsrc2=size={size}:rate={fps}",
                    "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", str(seconds),
                    "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-b:v", f"{bitrate_k}k",
                    "-maxrate", f"{bitrate_k}k", "-bufsize", f"{bitrate_k}k", "-g", str(int(fps * gop_s)), "-bf", "0",
                    "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "128k", "-f", "flv", path], check=True)


def scenario(a):
    token = a.token or open(a.token_file).read().strip()
    work = a.work or os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "senca-loadgen")
    os.makedirs(work, exist_ok=True)
    src = os.path.join(work, f"src_{a.size}_{a.bitrate}k_{a.fps}fps.flv")
    make_source(a.ffmpeg, src, a.bitrate, a.size, a.fps, a.gop, 30)
    tag = f"load{int(time.time())}"
    streams = []
    for i in range(a.publishers):
        s = api(a.api, token, "POST", "/streams", {"name": f"{tag}-{i}", "recording": {"enabled": a.record}})
        streams.append(s)
    log = open(os.path.join(work, "ffmpeg.log"), "ab")
    pubs = [Publisher(a.ffmpeg, src, f"{a.rtmp}/live/{s['ingest']['stream_key']}", log) for s in streams]
    for p in pubs:
        p.start()
    t_pub = time.monotonic()
    urls = []
    try:
        # HLS ができるまで待つ（最初の segment が閉じるまで）
        for s in streams:
            ttl = min(86_400, int(a.duration) + 600)
            pb = api(a.api, token, "POST", f"/streams/{s['id']}/playback-url", {"ttl_secs": ttl})
            urls.append(pb["playback"]["hls"])
        ready_deadline = time.monotonic() + 30
        for u in urls:
            while time.monotonic() < ready_deadline:
                try:
                    with urllib.request.urlopen(u, timeout=5) as r:
                        if r.status == 200 and b"#EXTINF" in r.read():
                            break
                except Exception:
                    pass
                time.sleep(0.5)
        startup = time.monotonic() - t_pub
        print(f"scenario: 送り手 {len(pubs)} 本（{a.size} {a.bitrate}k+128k、{a.fps}fps）、HLS 準備 {startup:.1f}s")

        stop = {"v": False}
        pub_events = {"restarts": 0, "died": 0}

        def churn_publishers():
            # 送り手の接続・切断（--pub-churn 秒ごとに 1 本を止めて、少し後に再開）
            while not stop["v"]:
                time.sleep(1)
                for p in pubs:
                    if not p.alive() and not stop["v"]:
                        pub_events["died"] += 1
                        p.start()
                if a.pub_churn and random.random() < 1.0 / a.pub_churn:
                    p = random.choice(pubs)
                    p.stop()
                    time.sleep(random.uniform(0.5, 2.0))
                    p.start()
                    pub_events["restarts"] += 1

        import threading
        th = threading.Thread(target=churn_publishers, daemon=True)
        th.start()
        st = run_viewers_multi(urls, a.viewers, a.duration, a.ramp, a.churn, a.timeout, a.procs)
        stop["v"] = True
        th.join()
        s = summarize(st, a.duration, a.viewers)
        s.update({"publishers": a.publishers, "source": {"size": a.size, "video_kbps": a.bitrate, "fps": a.fps,
                  "gop_s": a.gop}, "hls_ready_s": round(startup, 1), "publisher_restarts": pub_events["restarts"],
                  "publisher_unexpected_exits": pub_events["died"]})
        print_summary(s, "scenario: ")
        print(f"scenario: 送り手の再接続 {pub_events['restarts']}、想定外の終了 {pub_events['died']}")
        if a.out:
            with open(a.out, "w") as f:
                json.dump(s, f, indent=2, ensure_ascii=False)
        return s
    finally:
        for p in pubs:
            p.stop()
        if not a.keep_streams:
            for s in streams:
                try:
                    api(a.api, token, "DELETE", f"/streams/{s['id']}")
                except Exception:
                    pass


# ------------------------------------------------------------------ プロセスの標本


def proc_sample(pid, prev):
    with open(f"/proc/{pid}/stat") as f:
        st = f.read()
    fields = st[st.rindex(")") + 2:].split()
    utime, stime = int(fields[11]), int(fields[12])
    threads = int(fields[17])
    rss_kb = 0
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                rss_kb = int(line.split()[1])
    fds = os.listdir(f"/proc/{pid}/fd")
    socks = 0
    for fd in fds:
        try:
            if os.readlink(f"/proc/{pid}/fd/{fd}").startswith("socket:"):
                socks += 1
        except OSError:
            pass
    now = time.monotonic()
    ticks = utime + stime
    hz = os.sysconf("SC_CLK_TCK")
    cpu = None
    if prev:
        cpu = 100.0 * (ticks - prev[0]) / hz / max(1e-6, now - prev[1])
    return (ticks, now), {"rss_kb": rss_kb, "cpu_pct": None if cpu is None else round(cpu, 1), "fds": len(fds),
                          "sockets": socks, "threads": threads}


def sample(a):
    pid = a.pid
    prev = None
    t0 = time.time()
    with open(a.csv, "a") as f:
        if f.tell() == 0:
            f.write("unix_time,elapsed_s,rss_kb,cpu_pct,fds,sockets,threads,phase\n")
        while True:
            try:
                prev, s = proc_sample(pid, prev)
            except FileNotFoundError:
                f.write(f"{time.time():.0f},{time.time() - t0:.0f},,,,,,exited\n")
                f.flush()
                return 1
            phase = open(a.phase_file).read().strip() if a.phase_file and os.path.exists(a.phase_file) else ""
            f.write(f"{time.time():.0f},{time.time() - t0:.0f},{s['rss_kb']},{'' if s['cpu_pct'] is None else s['cpu_pct']},"
                    f"{s['fds']},{s['sockets']},{s['threads']},{phase}\n")
            f.flush()
            time.sleep(a.interval)


def analyze(a):
    rows = []
    with open(a.csv) as f:
        hdr = f.readline().strip().split(",")
        for line in f:
            r = dict(zip(hdr, line.strip().split(",")))
            rows.append(r)
    crashed = any(r.get("phase") == "exited" for r in rows)
    load = [r for r in rows if r.get("phase") == "load" and r.get("rss_kb")]
    idle_before = [r for r in rows if r.get("phase") == "idle-before" and r.get("rss_kb")]
    idle_after = [r for r in rows if r.get("phase") == "idle-after" and r.get("rss_kb")]
    res = {"samples": len(rows), "crashed": crashed}
    if load:
        t_load0 = float(load[0]["elapsed_s"])
        post = [r for r in load if float(r["elapsed_s"]) - t_load0 >= a.warmup]
        if len(post) >= 2:
            rss = [int(r["rss_kb"]) for r in post]
            k = max(1, min(3, len(rss) // 3))
            start = statistics.median(rss[:k])
            end = statistics.median(rss[-k:])
            growth = 100.0 * (end - start) / start
            xs = [float(r["elapsed_s"]) for r in post]
            mx, my = statistics.mean(xs), statistics.mean(rss)
            den = sum((x - mx) ** 2 for x in xs)
            slope = sum((x - mx) * (y - my) for x, y in zip(xs, rss)) / den if den else 0.0
            res.update({
                "warmup_s": a.warmup,
                "rss_after_warmup_kb": int(start), "rss_end_kb": int(end), "rss_growth_pct": round(growth, 2),
                "rss_slope_kb_per_h": round(slope * 3600, 1),
                "rss_max_kb": max(int(r["rss_kb"]) for r in load),
                "cpu_mean_pct": round(statistics.mean(float(r["cpu_pct"]) for r in load if r.get("cpu_pct")), 1),
                "threads_max_load": max(int(r["threads"]) for r in load),
                "sockets_max_load": max(int(r["sockets"]) for r in load),
            })
        else:
            res["note"] = "warm-up 後の標本が足りない"
    if idle_before and idle_after:
        fb, fa = int(idle_before[-1]["fds"]), int(idle_after[-1]["fds"])
        tb, ta = int(idle_before[-1]["threads"]), int(idle_after[-1]["threads"])
        sb, sa = int(idle_before[-1]["sockets"]), int(idle_after[-1]["sockets"])
        res.update({"fds_before": fb, "fds_after": fa, "sockets_before": sb, "sockets_after": sa,
                    "threads_before": tb, "threads_after": ta})
    ok = not crashed
    reasons = []
    if crashed:
        reasons.append("ノードが落ちた")
    if "rss_growth_pct" in res and res["rss_growth_pct"] > a.max_growth:
        ok = False
        reasons.append(f"warm-up 後の RSS 増分 {res['rss_growth_pct']}% > {a.max_growth}%")
    if "fds_after" in res and res["fds_after"] > res["fds_before"] + a.fd_slack:
        ok = False
        reasons.append(f"fd が残った（{res['fds_before']} → {res['fds_after']}）")
    if "sockets_after" in res and res["sockets_after"] > res["sockets_before"] + a.fd_slack:
        ok = False
        reasons.append(f"socket が残った（{res['sockets_before']} → {res['sockets_after']}）")
    res["result"] = "PASS" if ok else "FAIL"
    res["reasons"] = reasons
    print(json.dumps(res, ensure_ascii=False, indent=2))
    if a.out:
        with open(a.out, "w") as f:
            json.dump(res, f, ensure_ascii=False, indent=2)
    return 0 if ok else 1


# ------------------------------------------------------------------ main


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    v = sub.add_parser("viewers")
    v.add_argument("--url", action="append", required=True, help="署名付きの HLS の URL（何本でも）")
    v.add_argument("--viewers", type=int, default=100)
    v.add_argument("--duration", type=float, default=60)
    v.add_argument("--ramp", type=float, default=5, help="全員がつながるまでの秒数")
    v.add_argument("--churn", type=float, default=0, help="視聴の平均の長さ（秒、指数分布）。0 なら最後まで")
    v.add_argument("--timeout", type=float, default=10)
    v.add_argument("--procs", type=int, default=max(1, (os.cpu_count() or 2) // 4))
    v.add_argument("--out")

    s = sub.add_parser("scenario")
    s.add_argument("--api", required=True, help="例: http://127.0.0.1:8080/api/v1")
    s.add_argument("--token")
    s.add_argument("--token-file")
    s.add_argument("--rtmp", required=True, help="例: rtmp://127.0.0.1:1935")
    s.add_argument("--ffmpeg", default=os.environ.get("FFMPEG", os.path.expanduser("~/tools/ffmpeg")))
    s.add_argument("--publishers", type=int, default=4)
    s.add_argument("--viewers", type=int, default=200)
    s.add_argument("--duration", type=float, default=60)
    s.add_argument("--ramp", type=float, default=5)
    s.add_argument("--churn", type=float, default=0, help="視聴の平均の長さ（秒）。0 なら最後まで")
    s.add_argument("--pub-churn", type=float, default=0, help="平均この秒数ごとに送り手を 1 本つなぎ直す。0 なら無し")
    s.add_argument("--timeout", type=float, default=10)
    s.add_argument("--procs", type=int, default=max(1, (os.cpu_count() or 2) // 4))
    s.add_argument("--size", default="1280x720")
    s.add_argument("--bitrate", type=int, default=2000, help="映像の kbit/s")
    s.add_argument("--fps", type=int, default=30)
    s.add_argument("--gop", type=float, default=1.0, help="GOP の秒数（仕様 13.1 の基本は 1 秒）")
    s.add_argument("--record", action="store_true")
    s.add_argument("--keep-streams", action="store_true")
    s.add_argument("--work")
    s.add_argument("--out")

    p = sub.add_parser("sample")
    p.add_argument("--pid", type=int, required=True)
    p.add_argument("--csv", required=True)
    p.add_argument("--interval", type=float, default=10)
    p.add_argument("--phase-file", help="中身（idle-before・load・idle-after）を phase 列に書く")

    z = sub.add_parser("analyze")
    z.add_argument("--csv", required=True)
    z.add_argument("--warmup", type=float, default=600)
    z.add_argument("--max-growth", type=float, default=5.0)
    z.add_argument("--fd-slack", type=int, default=4)
    z.add_argument("--out")

    a = ap.parse_args()
    if a.cmd == "viewers":
        st = run_viewers_multi(a.url, a.viewers, a.duration, a.ramp, a.churn, a.timeout, a.procs)
        s_ = summarize(st, a.duration, a.viewers)
        print_summary(s_)
        if a.out:
            with open(a.out, "w") as f:
                json.dump(s_, f, indent=2, ensure_ascii=False)
        return 0 if s_["error_count"] == 0 else 1
    if a.cmd == "scenario":
        s_ = scenario(a)
        return 0 if s_["error_count"] == 0 else 1
    if a.cmd == "sample":
        return sample(a)
    if a.cmd == "analyze":
        return analyze(a)


if __name__ == "__main__":
    sys.exit(main())
