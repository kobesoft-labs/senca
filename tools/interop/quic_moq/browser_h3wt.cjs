// Chrome（playwright-core）で HTTP/3 と WebTransport（draft-02/07）を試す。
// 使い方: node browser_h3wt.cjs PORT SPKI_B64 CERT_SHA256_HEX OUT.json
const fs = require('fs'); const path = require('path'); const { execSync } = require('child_process');
const [port, spki, certHash, out] = process.argv.slice(2);
function findPw() { try { return require.resolve('playwright-core'); } catch {} const o = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim(); return path.dirname(o); }
const { chromium } = require(findPw());
(async () => {
  const checks = [];
  const check = (name, ok, detail='') => checks.push({name, ok: !!ok, detail: String(detail).slice(0,300)});
  const exe = fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined;
  const browser = await chromium.launch({ executablePath: exe, headless: true, args: [
    `--origin-to-force-quic-on=127.0.0.1:${port}`, `--ignore-certificate-errors-spki-list=${spki}`, '--enable-quic', '--quic-version=h3'] });
  const page = await browser.newPage();
  const base = `https://127.0.0.1:${port}`;
  try {
    const resp = await page.goto(base + '/', { timeout: 15000 });
    const proto = await page.evaluate(() => performance.getEntriesByType('navigation')[0].nextHopProtocol);
    check('H3 GET / status 200 over h3', resp.status() === 200 && proto === 'h3', `status=${resp.status()} proto=${proto}`);
    const r = await page.evaluate(async () => {
      const body = new Uint8Array(300000); for (let i = 0; i < body.length; i++) body[i] = i % 251;
      const res = await fetch('/echo', { method: 'POST', body });
      const got = new Uint8Array(await res.arrayBuffer());
      let ok = got.length === body.length; for (let i = 0; ok && i < got.length; i++) if (got[i] !== body[i]) ok = false;
      const big = await (await fetch('/bytes/2000000')).arrayBuffer();
      return { ok, len: got.length, big: big.byteLength, proto: performance.getEntriesByName(location.origin + '/echo')[0]?.nextHopProtocol };
    });
    check('H3 POST /echo 300KB roundtrip', r.ok, JSON.stringify(r));
    check('H3 GET /bytes/2000000', r.big === 2000000, JSON.stringify(r));
    const wt = await page.evaluate(async (hashHex) => {
      const hash = new Uint8Array(hashHex.match(/../g).map(h => parseInt(h, 16)));
      const res = {}; const to = (p, ms, what) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error('timeout: ' + what)), ms))]);
      const t = new WebTransport(location.origin + '/wt/echo', { serverCertificateHashes: [{ algorithm: 'sha-256', value: hash }] });
      await t.ready; res.ready = true;
      // bidi
      const bs = await t.createBidirectionalStream();
      const w = bs.writable.getWriter(); const msg = new TextEncoder().encode('hello webtransport '.repeat(1000));
      await w.write(msg); await w.close();
      const chunks = []; const rd = bs.readable.getReader(); for (;;) { const { value, done } = await to(rd.read(), 5000, 'bidi read'); if (done) break; chunks.push(...value); }
      res.bidi = chunks.length === msg.length && chunks.every((b, i) => b === msg[i]);
      // uni → uni back
      const us = await t.createUnidirectionalStream(); const uw = us.getWriter(); await uw.write(new TextEncoder().encode('uni-data')); await uw.close();
      const ir = t.incomingUnidirectionalStreams.getReader(); const { value: inc } = await to(ir.read(), 5000, 'incoming uni');
      const r2 = inc.getReader(); let s = ''; for (;;) { const { value, done } = await to(r2.read(), 5000, 'uni read'); if (done) break; s += new TextDecoder().decode(value); }
      res.uni = s;
      // datagram echo
      const dw = t.datagrams.writable.getWriter(); const dr = t.datagrams.readable.getReader();
      let got = null;
      for (let i = 0; i < 20 && !got; i++) {
        await dw.write(new TextEncoder().encode('dgram-' + i));
        const v = await Promise.race([dr.read(), new Promise(r => setTimeout(() => r(null), 200))]);
        if (v && v.value) got = new TextDecoder().decode(v.value.value ?? v.value);
      }
      res.datagram = got;
      t.close({ closeCode: 7, reason: 'bye' });
      res.closeInfo = await to(t.closed, 5000, 'closed').then(i => JSON.stringify(i)).catch(e => String(e));
      res.closed = true;
      return res;
    }, certHash).catch(e => ({ error: String(e) }));
    check('WT raw', true, JSON.stringify(wt));
    check('WebTransport session ready', wt.ready, JSON.stringify(wt));
    check('WebTransport bidi echo 19KB', wt.bidi, JSON.stringify(wt));
    check('WebTransport uni -> uni', wt.uni === 'uni-data', JSON.stringify(wt));
    check('WebTransport datagram echo', typeof wt.datagram === 'string' && wt.datagram.startsWith('dgram-'), JSON.stringify(wt));
    check('WebTransport close', wt.closed, JSON.stringify(wt));
  } catch (e) { check('browser run', false, e.stack || e); }
  fs.writeFileSync(out, JSON.stringify({ checks }, null, 1));
  await browser.close();
})();
