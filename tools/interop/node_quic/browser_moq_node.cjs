// Chrome（playwright-core）で senca-node の HTTP/3 と、WebTransport の上の MoQ draft-22 の購読を確かめる。
// 使い方: node browser_moq_node.cjs PORT SPKI_B64 CERT_SHA256_HEX OUT.json QUERY NAMESPACE TRACK COUNT
//   - https://127.0.0.1:PORT/healthz を HTTP/3 で開く（secure context。nextHopProtocol が h3 か）
//   - https://127.0.0.1:PORT/moq?QUERY に WebTransport でつなぎ、NAMESPACE の TRACK を SUBSCRIBE して
//     COUNT 個の object を受ける（最初の object が CMAF の moof で始まるか）
//   - 署名なし（/moq）の SUBSCRIBE が SUBSCRIBE_OK にならないか
// WebTransport・HTTP/3 は Chrome の実装（独立実装）、MoQ の符号化はこのファイルの最小実装。
const fs = require('fs'); const path = require('path'); const { execSync } = require('child_process');
const [port, spki, certHash, out, query, ns, track, count] = process.argv.slice(2);
function findPw() { try { return require.resolve('playwright-core'); } catch {} const o = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim(); return path.dirname(o); }
const { chromium } = require(findPw());
(async () => {
  const checks = [];
  const check = (name, ok, detail = '') => checks.push({ name, ok: !!ok, detail: String(detail).slice(0, 400) });
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [`--origin-to-force-quic-on=127.0.0.1:${port}`, `--ignore-certificate-errors-spki-list=${spki}`] });
  const page = await browser.newPage();
  try {
    const resp = await page.goto(`https://127.0.0.1:${port}/healthz`, { timeout: 15000 });
    const proto = await page.evaluate(() => performance.getEntriesByType('navigation')[0].nextHopProtocol);
    check('Chrome loads /healthz from senca-node over HTTP/3', resp && resp.status() === 200 && proto === 'h3', `status=${resp && resp.status()} protocol=${proto}`);
    const run = async (q, wantCount) => page.evaluate(async ([port, certHash, q, ns, track, count]) => {
      const res = { objects: 0, bytes: 0, groups: [], firstPayload: null };
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
      const t = new WebTransport(`https://127.0.0.1:${port}/moq${q ? '?' + q : ''}`, { serverCertificateHashes: [{ algorithm: 'sha-256', value: hash }], protocols: ['moqt-22'] });
      await to(t.ready, 8000, 'ready');
      res.ready = true; res.protocol = t.protocol;
      const ctl = await t.createUnidirectionalStream(); const cw = ctl.getWriter();
      await cw.write(msg(0x2f00, cat(vEnc(0x07), lp(bytesOf('chrome-js-min')))));
      const peerSetup = new Promise(async (resolve) => {
        const ir = t.incomingUnidirectionalStreams.getReader();
        for (;;) {
          const { value: s, done } = await ir.read(); if (done) break;
          (async () => {
            const r = s.getReader(); const rd = new Rd(); let state = 'type'; let hdr = null;
            for (;;) {
              const { value, done } = await r.read(); if (done) break; rd.push(value);
              for (;;) {
                if (state === 'type') {
                  const v = rd.vint(0); if (!v) break;
                  if (v[0] === 0x2f00) {
                    if (rd.buf.length < v[1] + 2) break; const len = (rd.buf[v[1]] << 8) | rd.buf[v[1] + 1];
                    if (rd.buf.length < v[1] + 2 + len) break; resolve(true); state = 'control'; break;
                  }
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
                  if (res.firstPayload === null) res.firstPayload = new TextDecoder().decode(rd.buf.slice(o + 4, o + 8));
                  res.objects++; res.bytes += ln[0]; rd.buf = rd.buf.slice(o + ln[0]);
                } else break;
              }
            }
          })();
        }
      });
      await to(peerSetup, 5000, 'server SETUP');
      res.setup = true;
      // SUBSCRIBE（Request ID 0、LOCATION_FILTER = AbsoluteStart(0,0)）
      const nsParts = ns.split('/');
      const body = cat(vEnc(0), vEnc(nsParts.length), ...nsParts.map(p => lp(bytesOf(p))), lp(bytesOf(track)), vEnc(1), vEnc(0x21), vEnc(2), vEnc(0), vEnc(0));
      const bs = await t.createBidirectionalStream(); const bw = bs.writable.getWriter(); await bw.write(msg(0x03, body));
      const br = bs.readable.getReader(); const { value: resp } = await to(br.read(), 5000, 'SUBSCRIBE response');
      res.subscribeResponse = resp ? resp[0] : null;
      if (count > 0) {
        const deadline = Date.now() + 10000;
        while (res.objects < count && Date.now() < deadline) await new Promise(r => setTimeout(r, 100));
      }
      t.close({ closeCode: 0, reason: 'done' });
      return res;
    }, [port, certHash, q, ns, track, wantCount]).catch(e => ({ error: String(e) }));
    const r = await run(query, Number(count));
    check('Chrome WebTransport session to senca-node /moq (moqt-22)', r.ready && r.protocol === 'moqt-22', JSON.stringify(r));
    check('MoQ SETUP exchanged over Chrome WebTransport', r.setup, JSON.stringify(r));
    check('signed SUBSCRIBE answered with SUBSCRIBE_OK (0x04)', r.subscribeResponse === 0x04, JSON.stringify(r));
    check(`Chrome received >= ${count} objects, first is a CMAF fragment (moof)`, r.objects >= Number(count) && r.firstPayload === 'moof', JSON.stringify({ objects: r.objects, bytes: r.bytes, first: r.firstPayload, groups: [...new Set(r.groups || [])] }));
    const u = await run('', 0);
    check('unsigned SUBSCRIBE over WebTransport is refused (no SUBSCRIBE_OK)', u.setup && u.subscribeResponse !== 0x04 && u.subscribeResponse !== null, JSON.stringify(u));
  } catch (e) { check('browser run', false, e.stack || e); }
  fs.writeFileSync(out, JSON.stringify({ checks }, null, 1));
  await browser.close();
})();
