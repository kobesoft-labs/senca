// 管理画面のログインとユーザーの管理（ADR-0017）をヘッドレスの Chrome で確かめる（tools/interop/console.sh の D から呼ぶ）。
//   node console_auth.cjs <work> で、<work>/auth-env.json を読み、結果を <work>/auth-result.json に書く。
//   auth-env.json: {a: {base, token}, b: {base, token}, idp}
//     a = 利用者のいない、試験用の IdP（tools/interop/oidc_mock.py）を「google」「corp」で設定したノード
//     b = --auth-require-mfa のノード
// 確かめること: 最初の管理者・深いリンクの保持・パスワード・TOTP の登録（QR）・MFA のログイン・回復コード・ユーザーの追加と仮のパスワード・
// 最後の管理者の 409・SSE での一覧の更新・先に済ませるパスワードの変更と MFA の登録・セッションの失効・期限切れの入り直し（画面を残す）・
// OIDC（成功と失敗）・失敗の上限の待ち時間・狭い画面。画面写真は <work>/auth-*.png。
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { execSync } = require('child_process');

const work = process.argv[2];
const env = JSON.parse(fs.readFileSync(path.join(work, 'auth-env.json'), 'utf8'));
function findPw() {
  try { return require.resolve('playwright-core'); } catch { /* 探す */ }
  const out = execSync("find / -path '*/node_modules/playwright-core/package.json' -not -path '/proc/*' 2>/dev/null | head -1").toString().trim();
  if (!out) throw new Error('playwright-core not found');
  return path.dirname(out);
}
const { chromium } = require(findPw());
const checks = [];
const check = (name, ok, detail = '') => { checks.push({ name: `auth: ${name}`, ok: !!ok, detail: String(detail).slice(0, 500) }); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** RFC 6238（SHA-1・6 桁・30 秒）。step は今の区切りからのずれ。 */
function totp(secret, step = 0) {
  const alpha = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = '';
  for (const c of secret.replace(/=+$/, '').toUpperCase()) bits += alpha.indexOf(c).toString(2).padStart(5, '0');
  const key = Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
  const ctr = Buffer.alloc(8);
  ctr.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000) + step));
  const hm = crypto.createHmac('sha1', key).update(ctr).digest();
  const o = hm[hm.length - 1] & 15;
  return String((hm.readUInt32BE(o) & 0x7fffffff) % 1000000).padStart(6, '0');
}

// 試験の中で意図して起こす失敗（[状態, URL]）
const EXPECTED = [
  [/40[13]|429/, /\/api\/v1\/auth\/(login|login\/mfa|me|exchange)$/],
  [/401/, /\/api\/v1\/(users|events|capabilities|auth\/sessions|streams)/], // 失効させたセッション
  [/404/, /\/favicon\.ico$/], // 試験用の IdP のページ
  [/409/, /\/api\/v1\/users\/[^/?]+$/],
  [/403/, /\/api\/v1\/(nodes|devices|tokens|secrets|users|grid|audit)/], // 管理者でない利用者の画面（ダッシュボードなど）
  [/403/, /\/api\/v1\/streams$/], // 先に済ませる手続きの間は、ほかの操作は 403（試験でわざと呼ぶ）
];

(async () => {
  const browser = await chromium.launch({ executablePath: fs.existsSync('/usr/bin/google-chrome') ? '/usr/bin/google-chrome' : undefined, headless: true });
  const errors = [];
  const newPage = async (opts = {}) => {
    const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 }, locale: 'ja-JP', timezoneId: 'Asia/Tokyo', acceptDownloads: true, ...opts });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
    page.on('console', (m) => {
      if (m.type() !== 'error') return;
      const url = (m.location() && m.location().url) || '';
      if (/Failed to load resource/.test(m.text()) && EXPECTED.some(([st, re]) => st.test(m.text()) && re.test(url))) return;
      errors.push(`console: ${m.text()} @ ${url}`);
    });
    return page;
  };
  const shot = (page, n) => page.screenshot({ path: path.join(work, `auth-${n}.png`), fullPage: true }).catch(() => {});
  const step = async (page, name, fn) => {
    try { await fn(); } catch (e) { check(name, false, e.message); if (page) await shot(page, `fail-${name.replace(/\W+/g, '_')}`); }
  };
  const A = env.a.base;
  const B = env.b.base;
  const PW = 'correct horse battery 42';
  const state = { secret: null, codes: [], adminId: null, temp: null };

  // ---------------------------------------------------------------- 最初の管理者（深いリンクを保つ）
  const page = await newPage();
  const bootFetch = (base, tok, method, p, body) => page.evaluate(async ([base, tok, method, p, body]) => {
    const r = await fetch(`${base}/api/v1${p}`, { method, headers: { Authorization: `Bearer ${tok}`, 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
    return { status: r.status, body: await r.json().catch(() => null) };
  }, [base, tok, method, p, body]);
  await step(page, 'first-admin setup', async () => {
    await page.goto(`${A}/console/#/streams`);
    await page.getByTestId('setup-token').fill('senca_wrong');
    await page.getByTestId('setup-next').click();
    await page.getByTestId('setup-error').locator('.alert').waitFor({ timeout: 5000 });
    await page.getByTestId('setup-token').fill(env.a.token);
    await page.getByTestId('setup-next').click();
    await page.getByTestId('setup-username').fill('admin');
    await page.getByTestId('setup-display-name').fill('Hanako Admin');
    await page.getByTestId('setup-password').fill('short');
    const weak = await page.getByTestId('password-strength').getAttribute('data-score');
    await page.getByTestId('setup-password').fill(PW);
    const strong = await page.getByTestId('password-strength').getAttribute('data-score');
    await page.getByTestId('setup-password2').fill(PW);
    await shot(page, '01-setup-account');
    await page.getByTestId('setup-create').click();
    await page.getByTestId('streams-table').or(page.locator('main .empty-state')).first().waitFor({ timeout: 15000 });
    const hash = await page.evaluate(() => location.hash);
    const tok = await page.evaluate(() => sessionStorage.getItem('senca.console.token'));
    check('first-admin setup: bad token rejected, strength meter, account created, signed in, deep link #/streams kept', hash === '#/streams' && /^sencas_/.test(tok || '') && weak === '0' && Number(strong) >= 3, `${hash} ${weak}/${strong}`);
  });

  // ---------------------------------------------------------------- アカウントのメニュー → MFA の登録
  await step(page, 'mfa enrollment', async () => {
    await page.getByTestId('user-menu').click();
    const ident = await page.getByTestId('identity').textContent();
    await page.getByTestId('menu-account').click();
    await page.getByTestId('account-mfa-setup').waitFor({ timeout: 8000 });
    check('account menu shows display name, tenant and sign-in method; opens the account page', /Hanako Admin/.test(ident) && /default/.test(ident) && /パスワード/.test(ident), ident);
    await page.getByTestId('account-mfa-setup').click();
    await page.getByTestId('mfa-password').fill(PW);
    await page.getByTestId('mfa-password-next').click();
    await page.getByTestId('mfa-qr').waitFor({ timeout: 8000 });
    state.secret = await page.getByTestId('mfa-secret').getAttribute('data-secret');
    await page.getByTestId('mfa-qr').screenshot({ path: path.join(work, 'auth-qr.png') });
    // QR の中身（otpauth://）は、ノードが返したものと同じはず。console.sh が PNG を読み戻して比べる
    const label = await page.evaluate(() => document.querySelector('[data-testid="mfa-qr"]').getAttribute('data-version'));
    fs.writeFileSync(path.join(work, 'auth-qr-secret.txt'), state.secret);
    await shot(page, '02-mfa-qr');
    await page.getByTestId('mfa-code').fill(totp(state.secret));
    await page.getByTestId('recovery-codes').waitFor({ timeout: 8000 });
    state.codes = await page.getByTestId('recovery-code').allTextContents();
    const doneDisabled = await page.getByTestId('recovery-done').isDisabled();
    await shot(page, '03-recovery-codes');
    await page.getByTestId('recovery-saved').check();
    await page.getByTestId('recovery-done').click();
    await page.locator('[data-testid="mfa-status"].ok').waitFor({ timeout: 8000 });
    check('MFA enrollment: QR (local) + secret, TOTP code auto-submits, 10 recovery codes, "I saved them" gates Done, status On', state.codes.length === 10 && doneDisabled && !!label, `${state.codes.length} codes, qr v${label}`);
  });

  const logout = async (p) => {
    await p.getByTestId('user-menu').click();
    await p.getByTestId('logout').click();
    await p.getByTestId('login-form').waitFor({ timeout: 8000 });
  };
  const signIn = async (p, user, pw) => {
    await p.getByTestId('login-username').fill(user);
    await p.getByTestId('login-password').fill(pw);
    await p.getByTestId('login-submit-password').click();
  };

  // ---------------------------------------------------------------- パスワード → MFA のログイン、回復コード
  await step(page, 'mfa login', async () => {
    await logout(page);
    const sso = await page.locator('[data-testid^="sso-"]').count();
    check('logout calls /auth/logout and returns to the sign-in page (SSO buttons + password form)', sso === 2 && await page.evaluate(() => sessionStorage.length === 0), `sso=${sso}`);
    await signIn(page, 'admin', 'wrong password!!');
    await page.getByTestId('login-error').locator('.alert').waitFor({ timeout: 8000 });
    const errText = await page.getByTestId('login-error').textContent();
    await shot(page, '04-login-error');
    await signIn(page, 'admin', PW);
    await page.getByTestId('mfa-code').waitFor({ timeout: 8000 });
    await shot(page, '05-login-mfa');
    await page.getByTestId('mfa-code').fill(totp(state.secret, 1));
    await page.getByTestId('user-menu').waitFor({ timeout: 10000 });
    check('password login: generic error, then MFA step with 6-digit code → console', /ユーザー名かパスワードが違います/.test(errText), errText);
    await logout(page);
    await signIn(page, 'admin', PW);
    await page.getByTestId('mfa-use-recovery').click();
    await page.getByTestId('mfa-recovery').fill(state.codes[0]);
    await page.getByTestId('mfa-submit').click();
    await page.getByTestId('user-menu').waitFor({ timeout: 10000 });
    check('recovery code login', true);
  });

  // ---------------------------------------------------------------- ユーザー: 追加・仮のパスワード・最後の管理者・SSE
  await step(page, 'users', async () => {
    await page.goto(`${A}/console/#/users`);
    await page.getByTestId('users-table').waitFor({ timeout: 8000 });
    const me = await bootFetch(A, env.a.token, 'GET', '/users');
    state.adminId = me.body.items.find((u) => u.username === 'admin').id;
    await page.getByTestId('users-create').click();
    await page.getByTestId('user-username').fill('op1');
    await page.getByTestId('user-display-name').fill('Operator One');
    await page.getByTestId('role-operator').check();
    await shot(page, '06-user-create');
    await page.getByTestId('user-create-submit').click();
    await page.getByTestId('reveal-dialog').waitFor({ timeout: 8000 });
    state.temp = await page.getByTestId('temp-password').inputValue();
    await shot(page, '07-user-reveal');
    await page.getByTestId('reveal-done').click();
    await page.getByTestId('user-open-Operator One').or(page.getByTestId('user-open-op1')).first().waitFor({ timeout: 8000 });
    check('create user: operator preset, temporary password revealed once', state.temp && state.temp.length === 20, `len=${state.temp && state.temp.length}`);
    // 最後の管理者を閲覧者にしようとする → 409 の説明
    await page.getByTestId('user-open-admin').or(page.getByTestId('user-open-Hanako Admin')).first().click();
    await page.getByTestId('role-viewer').check();
    await page.getByTestId('user-edit-submit').click();
    await page.getByTestId('last-admin-inline').waitFor({ timeout: 8000 });
    const msg = await page.getByTestId('last-admin-inline').textContent();
    await shot(page, '08-last-admin');
    await page.keyboard.press('Escape');
    check('role change of the last admin → 409 explained (make someone else admin first)', /最後の管理者/.test(msg) && /ほかのユーザーを管理者に/.test(msg), msg);
    // ほかの主体が作ったユーザーが、読み直さずに出る（SSE の user.created）
    await bootFetch(A, env.a.token, 'POST', '/users', { username: 'viewer-sse', roles: ['viewer'], password: 'viewer password 1' });
    await page.getByTestId('user-open-viewer-sse').waitFor({ timeout: 10000 });
    check('users list updates live via SSE (user.created)', true);
    // 絞り込み
    await page.getByTestId('users-filter-role').selectOption('operator');
    const rows = await page.locator('[data-testid="users-table"] tbody tr').count();
    await page.getByTestId('users-filter-clear').click();
    check('role filter', rows === 1, `rows=${rows}`);
    await shot(page, '09-users');
  });

  // ---------------------------------------------------------------- 先に済ませるパスワードの変更
  const p2 = await newPage();
  await step(p2, 'forced password change', async () => {
    await p2.goto(`${A}/console/#/recordings`);
    await signIn(p2, 'op1', state.temp);
    await p2.getByTestId('forced-page').waitFor({ timeout: 8000 });
    const blocked = await p2.evaluate(async () => (await fetch('/api/v1/streams', { headers: { Authorization: `Bearer ${sessionStorage.getItem('senca.console.token')}` } })).status);
    await shot(p2, '10-forced-password');
    await p2.getByTestId('forced-new').fill('operator new password 1');
    await p2.getByTestId('forced-new2').fill('operator new password 1');
    await p2.getByTestId('forced-submit').click();
    await p2.getByTestId('user-menu').waitFor({ timeout: 10000 });
    const hash = await p2.evaluate(() => location.hash);
    const usersNav = await p2.getByTestId('nav-users').count();
    check('forced password change: other APIs 403 until done, then console at the deep link; non-admin has no Users nav', blocked === 403 && hash === '#/recordings' && usersNav === 0, `${blocked} ${hash} nav=${usersNav}`);
  });

  // ---------------------------------------------------------------- セッションの一覧と失効
  await step(page, 'sessions', async () => {
    // もう 1 つ別の端末のセッション（API で）
    const other = await page.evaluate(async (pw) => {
      const r = await fetch('/api/v1/auth/login', { method: 'POST', headers: { 'Content-Type': 'application/json', 'User-Agent': 'x' }, body: JSON.stringify({ username: 'viewer-sse', password: pw }) });
      return r.status;
    }, 'viewer password 1');
    await page.goto(`${A}/console/#/account`);
    await page.getByTestId('sessions-table').waitFor({ timeout: 8000 });
    // 管理者の 2 つ目のセッション: 回復コードで
    const p3 = await newPage({ userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1' });
    await p3.goto(`${A}/console/`);
    await signIn(p3, 'admin', PW);
    await p3.getByTestId('mfa-use-recovery').click();
    await p3.getByTestId('mfa-recovery').fill(state.codes[1]);
    await p3.getByTestId('mfa-submit').click();
    await p3.getByTestId('user-menu').waitFor({ timeout: 10000 });
    await page.getByRole('button', { name: '再読み込み' }).first().click();
    await page.locator('[data-testid="sessions-table"] tbody tr').nth(1).waitFor({ timeout: 8000 });
    const before = await page.locator('[data-testid="sessions-table"] tbody tr').count();
    const iphone = await page.getByTestId('sessions-table').textContent();
    await shot(page, '11-account-sessions');
    await page.getByTestId('session-revoke').first().click();
    await page.getByTestId('confirm-ok').click();
    await page.waitForFunction((n) => document.querySelectorAll('[data-testid="sessions-table"] tbody tr').length === n - 1, before, { timeout: 8000 });
    // 失効した端末は次の操作で入り直しのダイアログになる
    await p3.goto(`${A}/console/#/streams`);
    await p3.getByTestId('reauth-dialog').or(p3.getByTestId('login-page')).first().waitFor({ timeout: 10000 });
    check('sessions list (device from user agent, current marker) and revoke another device', other === 200 && before >= 2 && /Safari（iOS）/.test(iphone), `before=${before}`);
    await p3.context().close();
  });

  // ---------------------------------------------------------------- 期限切れ → 入り直し（画面を残す）
  await step(page, 'reauth', async () => {
    await page.goto(`${A}/console/#/users`);
    await page.getByTestId('users-table').waitFor({ timeout: 8000 });
    await page.getByTestId('users-search').fill('op');
    await sleep(300);
    const r = await bootFetch(A, env.a.token, 'DELETE', `/users/${state.adminId}/sessions`);
    await page.getByTestId('users-refresh').click();
    await page.getByTestId('reauth-dialog').waitFor({ timeout: 10000 });
    await shot(page, '12-reauth');
    await page.getByTestId('reauth-password').fill(PW);
    await page.getByTestId('reauth-submit').click();
    await page.getByTestId('reauth-use-recovery').click();
    await page.getByTestId('reauth-recovery').fill(state.codes[2]);
    await page.getByTestId('reauth-submit').click();
    await page.getByTestId('reauth-dialog').waitFor({ state: 'detached', timeout: 10000 });
    await sleep(500);
    const kept = await page.getByTestId('users-search').inputValue();
    const rows = await page.locator('[data-testid="users-table"] tbody tr').count();
    const hash = await page.evaluate(() => location.hash);
    check('session expiry mid-use → re-login dialog (password + recovery code), page state kept, request retried', r.status === 204 && kept === 'op' && rows >= 1 && hash === '#/users', `${r.status} kept=${kept} rows=${rows} ${hash}`);
  });

  // ---------------------------------------------------------------- OIDC
  const p4 = await newPage();
  await step(p4, 'oidc', async () => {
    const next = (body) => p4.evaluate(async ([idp, body]) => { await fetch(`${idp}/_next`, { method: 'POST', body: JSON.stringify(body) }); }, [env.idp, body]);
    await p4.goto(`${A}/console/#/recordings`);
    await p4.getByTestId('sso-google').waitFor({ timeout: 8000 });
    await shot(p4, '13-login-sso');
    await p4.goto(`${env.idp}/.well-known/openid-configuration`);
    await next({ claims: { sub: 'g-alice', email: 'alice@example.com', email_verified: true, name: 'Alice Example', groups: ['senca-admins'] } });
    await p4.goto(`${A}/console/#/recordings`);
    await p4.getByTestId('sso-google').click();
    await p4.getByTestId('user-menu').waitFor({ timeout: 15000 });
    const hash = await p4.evaluate(() => location.hash);
    await p4.getByTestId('user-menu').click();
    const method = await p4.getByTestId('identity-method').textContent();
    await p4.keyboard.press('Escape');
    check('OIDC login via the mock IdP → console at the deep link (return_to), sign-in method SSO', hash === '#/recordings' && /SSO/.test(method), `${hash} ${method}`);
    await logout(p4);
    await p4.goto(`${env.idp}/.well-known/openid-configuration`);
    await next({ fault: 'deny' });
    await p4.goto(`${A}/console/`);
    await p4.getByTestId('sso-google').click();
    await p4.getByTestId('oidc-error').waitFor({ timeout: 15000 });
    const code = await p4.getByTestId('oidc-error').getAttribute('data-code');
    const text = await p4.getByTestId('oidc-error').textContent();
    const retry = await p4.getByTestId('oidc-retry').count();
    const url = p4.url();
    await shot(p4, '14-oidc-error');
    check('OIDC error page (#/auth/complete?error=…): explanation, retry with the same provider, code removed from the URL', code === 'access_denied' && /取り消されました/.test(text) && retry === 1 && !/error=/.test(url), `${code} ${url}`);
  });

  // ---------------------------------------------------------------- 失敗の上限
  await step(p4, 'throttle', async () => {
    await p4.getByTestId('oidc-back').click();
    for (let i = 0; i < 8; i++) {
      await signIn(p4, 'lockme', `wrong password ${i}!!`);
      const t = await Promise.race([
        p4.getByTestId('login-throttled').waitFor({ timeout: 6000 }).then(() => 'throttled'),
        p4.locator('[data-testid="login-error"] .alert.error').waitFor({ timeout: 6000 }).then(() => 'error'),
      ]).catch(() => 'none');
      if (t === 'throttled') break;
      await sleep(150);
    }
    await p4.getByTestId('login-throttled').waitFor({ timeout: 3000 });
    const cd = await p4.getByTestId('login-countdown').textContent();
    const disabled = await p4.getByTestId('login-submit-password').isDisabled();
    await shot(p4, '15-login-throttled');
    check('429 throttling: message with Retry-After countdown, submit disabled', /^\d+:\d\d$/.test(cd) && disabled, cd);
  });

  // ---------------------------------------------------------------- 先に済ませる MFA の登録（--auth-require-mfa）
  const p5 = await newPage();
  await step(p5, 'forced mfa', async () => {
    await p5.goto(`${B}/console/`);
    const r = await p5.evaluate(async ([base, tok]) => (await fetch(`${base}/api/v1/users`, { method: 'POST', headers: { Authorization: `Bearer ${tok}`, 'Content-Type': 'application/json' }, body: JSON.stringify({ username: 'mfa-user', password: 'mfa user password', roles: ['operator'] }) })).status, [B, env.b.token]).catch((e) => e.message);
    await p5.reload();
    await p5.getByTestId('login-form').waitFor({ timeout: 8000 });
    await signIn(p5, 'mfa-user', 'mfa user password');
    await p5.getByTestId('forced-page').waitFor({ timeout: 8000 });
    await p5.getByTestId('mfa-qr').waitFor({ timeout: 8000 });
    await shot(p5, '16-forced-mfa');
    const secret = await p5.getByTestId('mfa-secret').getAttribute('data-secret');
    await p5.getByTestId('mfa-code').fill(totp(secret));
    await p5.getByTestId('recovery-saved').check();
    await p5.getByTestId('recovery-done').click();
    await p5.getByTestId('user-menu').waitFor({ timeout: 10000 });
    check('forced MFA enrollment (--auth-require-mfa): no second password prompt, QR → code → recovery codes → console', r === 201, String(r));
  });

  // ---------------------------------------------------------------- 狭い画面
  await step(null, 'mobile', async () => {
    const m = await newPage({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
    await m.goto(`${A}/console/`);
    await m.getByTestId('login-form').waitFor({ timeout: 8000 });
    const w1 = await m.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    await shot(m, '17-mobile-login');
    await signIn(m, 'admin', PW);
    await m.getByTestId('mfa-use-recovery').click();
    await m.getByTestId('mfa-recovery').fill(state.codes[3]);
    await m.getByTestId('mfa-submit').click();
    await m.getByTestId('user-menu').waitFor({ timeout: 10000 });
    const widths = {};
    for (const r of ['/users', '/account']) {
      await m.goto(`${A}/console/#${r}`);
      await m.locator('main h1').first().waitFor({ timeout: 8000 });
      await sleep(800);
      widths[r] = await m.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
      await shot(m, `18-mobile${r.replace('/', '-')}`);
    }
    await m.goto(`${A}/console/#/users`);
    await m.getByTestId('users-table').waitFor({ timeout: 8000 });
    const card = await m.locator('[data-testid="users-table"] tbody tr').first().evaluate((tr) => getComputedStyle(tr).display);
    check('mobile 390px: login, users and account pages do not scroll sideways; users table becomes cards', w1 <= 1 && Object.values(widths).every((w) => w <= 1) && card === 'block', JSON.stringify({ w1, widths, card }));
    await m.context().close();
  });

  const missing = await page.evaluate(() => window.__sencaI18nMissing()).catch((e) => [e.message]);
  check('no missing translation keys', missing.length === 0, missing.join(', '));
  check('no unexpected console errors', errors.length === 0, errors.join(' | '));
  fs.writeFileSync(path.join(work, 'auth-result.json'), JSON.stringify({ checks }, null, 1));
  await browser.close();
})().catch((e) => {
  checks.push({ name: 'auth: runner', ok: false, detail: e.stack });
  fs.writeFileSync(path.join(work, 'auth-result.json'), JSON.stringify({ checks }, null, 1));
  process.exit(0);
});
