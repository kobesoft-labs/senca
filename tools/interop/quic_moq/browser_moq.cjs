// Chrome（playwright-core）で WebTransport の上に MoQ draft-22 の最小の subscriber を動かす。
// 使い方: node browser_moq.cjs PORT SPKI_B64 CERT_SHA256_HEX OUT.json NAMESPACE TRACK COUNT PAGE_PORT
// （WebTransport は secure context が要るので、PAGE_PORT の HTTP/3 server の頁を開いてから接続する）
// WebTransport は Chrome の実装（独立実装）、MoQ の符号化はこのファイルの最小実装。
const fs = require('fs'); const path = require('path'); const { execSync } = require('child_process');
const [port, spki, certHash, out, ns, track, count, pagePort] = process.argv.slice(2);
function findPw() { try { return require.resolve('playwright-core'); } catch {} const o = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim(); return path.dirname(o); }
const { chromium } = require(findPw());
(async () => {
  const checks = [];
  const check = (name, ok, detail = '') => checks.push({ name, ok: !!ok, detail: String(detail).slice(0, 400) });
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [`--origin-to-force-quic-on=127.0.0.1:${port},127.0.0.1:${pagePort}`, `--ignore-certificate-errors-spki-list=${spki}`] });
  const page = await browser.newPage();
  try {
    await page.goto(`https://127.0.0.1:${pagePort}/`, { timeout: 15000 });
    const r = await page.evaluate(async ([port, certHash, ns, track, count]) => {
      const res = { objects: 0, bytes: 0, groups: [] };
      const to = (p, ms, what) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error('timeout: ' + what)), ms))]);
      // draft-22 の varint（先頭の 1 の数で長さ）
      const vEnc = (v) => {
        v = BigInt(v);
        for (let n = 1; n <= 8; n++) {
          const bits = BigInt(n === 8 ? 56 : 7 * n);
          if (v < (1n << bits)) {
            const b = new Uint8Array(n);
            for (let i = n - 1; i >= 0; i--) { b[i] = Number(v & 0xffn); v >>= 8n; }
            b[0] |= (0xff << (8 - (n - 1))) & 0xff;
            return b;
          }
        }
        const b = new Uint8Array(9); b[0] = 0xff; for (let i = 8; i >= 1; i--) { b[i] = Number(v & 0xffn); v >>= 8n; } return b;
      };
      class Rd { constructor() { this.buf = new Uint8Array(0); } push(d) { const n = new Uint8Array(this.buf.length + d.length); n.set(this.buf); n.set(d, this.buf.length); this.buf = n; }
        vint(off) { if (off >= this.buf.length) return null; const f = this.buf[off]; let n = 1; while (n < 9 && (f & (0x80 >> (n - 1)))) n++; if (n > 8) n = 9; if (off + n > this.buf.length) return null;
          let v = n === 9 ? 0n : BigInt(f & (0xff >> n)); for (let i = 1; i < n; i++) v = (v << 8n) | BigInt(this.buf[off + i]); return [Number(v), n]; } };
      const cat = (...a) => { const n = new Uint8Array(a.reduce((s, x) => s + x.length, 0)); let o = 0; for (const x of a) { n.set(x, o); o += x.length; } return n; };
      const msg = (type, body) => cat(vEnc(type), new Uint8Array([body.length >> 8, body.length & 0xff]), body);
      const bytesOf = (s) => new TextEncoder().encode(s);
      const lp = (b) => cat(vEnc(b.length), b);
      const hash = new Uint8Array(certHash.match(/../g).map(h => parseInt(h, 16)));
      const t = new WebTransport(`https://127.0.0.1:${port}/moq`, { serverCertificateHashes: [{ algorithm: 'sha-256', value: hash }], protocols: ['moqt-22'] });
      await to(t.ready, 8000, 'ready');
      res.ready = true; res.protocol = t.protocol;
      // 制御 stream: SETUP（MOQT_IMPLEMENTATION 0x07 のみ）
      const ctl = await t.createUnidirectionalStream(); const cw = ctl.getWriter();
      await cw.write(msg(0x2f00, cat(vEnc(0x07), lp(bytesOf('chrome-js-min')))));
      // 相手の SETUP と object を受ける
      const peerSetup = new Promise(async (resolve, reject) => {
        const ir = t.incomingUnidirectionalStreams.getReader();
        for (;;) {
          const { value: s, done } = await ir.read(); if (done) break;
          (async () => {
            const r = s.getReader(); const rd = new Rd(); let state = 'type'; let hdr = null; let prevId = -1; let off = 0;
            for (;;) {
              const { value, done } = await r.read(); if (done) break; rd.push(value);
              for (;;) {
                if (state === 'type') {
                  const v = rd.vint(0); if (!v) break;
                  if (v[0] === 0x2f00) { // 相手の制御 stream
                    if (rd.buf.length < v[1] + 2) break; const len = (rd.buf[v[1]] << 8) | rd.buf[v[1] + 1];
                    if (rd.buf.length < v[1] + 2 + len) break; resolve(true); state = 'control'; break;
                  }
                  // SUBGROUP_HEADER: flags, alias, group, [subgroup], [priority]
                  const flags = v[0]; let o = v[1]; const a = rd.vint(o); if (!a) break; o += a[1]; const g = rd.vint(o); if (!g) break; o += g[1];
                  if ((flags & 0x06) === 0x04) { const sg = rd.vint(o); if (!sg) break; o += sg[1]; }
                  if (!(flags & 0x20)) { if (o >= rd.buf.length) break; o += 1; }
                  hdr = { flags, group: g[0] }; rd.buf = rd.buf.slice(o); state = 'obj'; res.groups.push(g[0]);
                } else if (state === 'obj') {
                  let o = 0; const d = rd.vint(o); if (!d) break; o += d[1];
                  if (hdr.flags & 0x01) { const pl = rd.vint(o); if (!pl) break; o += pl[1] + pl[0]; if (o > rd.buf.length) break; }
                  const ln = rd.vint(o); if (!ln) break; o += ln[1];
                  if (ln[0] === 0) { const st = rd.vint(o); if (!st) break; o += st[1]; }
                  if (rd.buf.length < o + ln[0]) break;
                  res.objects++; res.bytes += ln[0]; rd.buf = rd.buf.slice(o + ln[0]);
                } else break;
              }
            }
          })();
        }
      });
      await to(peerSetup, 5000, 'server SETUP');
      res.setup = true;
      // SUBSCRIBE（Request ID 0、namespace、track、パラメーター: LOCATION_FILTER=AbsoluteStart(0,0)）
      const nsParts = ns.split('/');
      const body = cat(vEnc(0), vEnc(nsParts.length), ...nsParts.map(p => lp(bytesOf(p))), lp(bytesOf(track)), vEnc(1), vEnc(0x21), vEnc(2), vEnc(0), vEnc(0));
      const bs = await t.createBidirectionalStream(); const bw = bs.writable.getWriter(); await bw.write(msg(0x03, body));
      const br = bs.readable.getReader(); const { value: resp } = await to(br.read(), 5000, 'SUBSCRIBE_OK');
      res.subscribeResponse = resp[0];
      const deadline = Date.now() + 8000;
      while (res.objects < count && Date.now() < deadline) await new Promise(r => setTimeout(r, 100));
      t.close({ closeCode: 0, reason: 'done' });
      return res;
    }, [port, certHash, ns, track, Number(count)]).catch(e => ({ error: String(e) }));
    check('browser WebTransport session to MoQ relay', r.ready, JSON.stringify(r));
    check('MoQ SETUP exchanged over browser WebTransport', r.setup, JSON.stringify(r));
    check('SUBSCRIBE_OK (type 0x04) over browser WebTransport', r.subscribeResponse === 0x04, JSON.stringify(r));
    check(`objects received in Chrome (>= ${count})`, r.objects >= Number(count), JSON.stringify(r));
  } catch (e) { check('browser run', false, e.stack || e); }
  fs.writeFileSync(out, JSON.stringify({ checks }, null, 1));
  await browser.close();
})();
