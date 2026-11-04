// SENCA 管理画面の入口。ハッシュの経路（#/streams など）で画面を切り替える。
// 使う API は公開の /api/v1 だけ（REQ-002）。トークン・セッションの値は sessionStorage にだけ置く。
//
// 画面のモジュールは render(main, params, ctx) を持ち、片付けの関数を返してよい。
// ctx = { caps, navigate(path), setCrumbs(items) }。経路の一覧は js/nav.js。
//
// ログインの流れ: ログインの画面（js/views/login.js）→ GET /auth/me → pending があれば先に済ませる手続き
// （js/views/auth-forced.js）→ GET /capabilities → 外枠と画面。OIDC から戻ったところ（#/auth/complete）は外枠の前に扱う。
// 使っている途中で 401 になったら、画面を残したまま入り直しのダイアログ（js/views/reauth.js）を出し、待っていた要求を送り直す。

import { h, replace } from './lib/dom.js';
import { on } from './lib/bus.js';
import { t, onLangChange } from './i18n/index.js';
import './lib/theme.js';
import { toast } from './ui/toast.js';
import { closeFloating } from './ui/overlay.js';
import { skeleton } from './ui/feedback.js';
import { getToken, clearToken, isSessionToken, setReauthHandler } from './api/client.js';
import { system } from './api/resources.js';
import { auth } from './api/auth.js';
import { EventStream } from './api/sse.js';
import { renderLogin, renderAuthComplete } from './views/login.js';
import { renderForced } from './views/auth-forced.js';
import { reauthDialog } from './views/reauth.js';
import { getMe, setMe, guessMe, setProviderNames } from './lib/session.js';
import { findRoute, navItem } from './nav.js';
import { buildShell } from './shell.js';
import { openPalette } from './palette.js';
import { installShortcuts } from './shortcuts.js';
import { errorBox } from './lib/ui.js';
import './lib/activity.js';

const state = {
  caps: null,
  sse: null,
  cleanup: null,
  renderSeq: 0,
  shell: null,
  loginMessage: null,
  loginKind: 'info',
  /** 今の画面: 'login'・'forced'・'console' */
  screen: null,
  /** 先に済ませる手続きの間だけ覚えておくパスワード（メモリの中だけ） */
  password: null,
  login: null,
  reauth: null,
};

export function navigate(path) {
  const p = path.replace(/^#/, '');
  if (location.hash !== `#${p}`) location.hash = p;
  else route();
}

const palette = () => openPalette({ navigate, logout });

function runCleanup() {
  if (state.cleanup) {
    try { state.cleanup(); } catch { /* 何もしない */ }
    state.cleanup = null;
  }
}

function mountShell() {
  if (state.shell) state.shell.destroy();
  state.shell = buildShell(document.getElementById('app'), { caps: state.caps, logout, palette });
}

async function route({ focus = true } = {}) {
  if (!getToken()) {
    showLogin();
    return;
  }
  if (!state.shell) mountShell();
  closeFloating();
  const path = location.hash.replace(/^#/, '').split('?')[0] || '/';
  const r = findRoute(path);
  const seq = ++state.renderSeq;
  runCleanup();
  const { main } = state.shell;
  state.shell.setActive(r ? r.nav : null);
  main.classList.toggle('full', !!(r && (r.nav === 'pipelines' || r.nav === 'production')));
  if (!r) {
    replace(main, h('div', { class: 'coming-soon' },
      h('h1', null, t('shell.notFound')),
      h('p', { class: 'muted' }, t('shell.notFoundBody')),
      h('a', { class: 'btn primary', href: '#/' }, t('shell.backToDashboard'))));
    document.title = `${t('shell.notFound')} — ${t('shell.title')}`;
    return;
  }
  replace(main, h('div', { class: 'route-loading' }, skeleton({ kind: 'lines', rows: 5 })));
  try {
    const mod = await r.load();
    if (seq !== state.renderSeq) return;
    replace(main);
    const cleanup = await mod.render(main, r.params, { caps: state.caps, navigate, setCrumbs: (c) => state.shell.setCrumbs(c), nav: r.nav });
    if (seq !== state.renderSeq) {
      if (typeof cleanup === 'function') cleanup();
      return;
    }
    state.cleanup = typeof cleanup === 'function' ? cleanup : null;
    const h1 = main.querySelector('h1');
    const item = r.nav ? navItem(r.nav) : null;
    document.title = `${h1 ? h1.textContent : item ? t(`nav.${item.id}`) : 'SENCA'} — ${t('shell.title')}`;
    // 画面が変わったことを読み上げで分かるように、本文へフォーカスを移す（最初の表示では移さない）
    if (focus && document.activeElement && document.activeElement !== document.body && !main.contains(document.activeElement)) main.focus({ preventScroll: true });
    window.scrollTo(0, 0);
  } catch (e) {
    replace(main, errorBox(e, { title: t('shell.renderFailed') }));
  }
}

function leaveConsole() {
  runCleanup();
  if (state.sse) state.sse.stop();
  state.sse = null;
  if (state.shell) state.shell.destroy();
  state.shell = null;
  if (state.login) state.login.destroy();
  state.login = null;
  closeFloating();
}

function showLogin(message = state.loginMessage, kind = state.loginKind) {
  leaveConsole();
  state.screen = 'login';
  state.loginMessage = message || null;
  state.loginKind = kind || 'info';
  state.login = renderLogin(document.getElementById('app'), {
    message,
    messageKind: kind,
    onLogin: (r) => { state.loginMessage = null; enter(r); },
  });
  document.title = `${t('login.title')} — ${t('shell.title')}`;
}

function showForced(me, password = state.password) {
  leaveConsole();
  state.screen = 'forced';
  state.password = password || null;
  renderForced(document.getElementById('app'), {
    me,
    password,
    onDone: (next) => { state.password = null; enter({ me: next }); },
    onLogout: () => logout(),
  });
  document.title = `${t('auth.forced.docTitle')} — ${t('shell.title')}`;
}

/**
 * ログインした後（とページを開き直したとき）: 主体を確かめ、先に済ませる手続きがあればそちらへ、無ければ外枠と画面へ。
 * @param {{me?: object, password?: string, returnTo?: string|null}} [r]
 */
async function enter({ me, password, returnTo } = {}) {
  if (!me) {
    try {
      ({ data: me } = await auth.me({ quiet401: true }));
    } catch (e) {
      if (e.status === 404) me = guessMe(); // /auth/me の無い古いノード
      else if (e.status === 401) { clearToken(); setMe(null); showLogin(t('login.storedInvalid'), 'warn'); return; }
      else { showLogin(t('login.cannotConnect', { reason: e.title || e.message }), 'error'); return; }
    }
  }
  // IdP の表示名（アカウントのメニュー・セッション・ユーザーの一覧で ID の代わりに出す）
  try { setProviderNames((await auth.providers()).data.oidc); } catch { /* ID のまま出す */ }
  setMe(me);
  if (me.pending && me.pending.length) { showForced(me, password); return; }
  state.password = null;
  if (returnTo) history.replaceState(history.state, '', `${location.pathname}${location.search}${returnTo}`);
  try {
    const { data } = await system.capabilities({ quiet401: true });
    state.caps = data;
  } catch (e) {
    if (e.status === 401) { clearToken(); setMe(null); showLogin(t('login.storedInvalid'), 'warn'); return; }
    if (e.status === 403 && e.details && e.details.reason === 'pending') { showForced({ ...me, pending: e.details.pending || [] }, password); return; }
    showLogin(t('login.cannotConnect', { reason: e.title || e.message }), 'error');
    return;
  }
  startSession();
}

function startSession() {
  if (state.login) state.login.destroy();
  state.login = null;
  state.screen = 'console';
  state.sse = new EventStream();
  mountShell();
  state.sse.start();
  route({ focus: false });
}

/** ログアウト: セッションならノードでも失効させ、値を消してログインの画面へ（行き先はダッシュボードに戻す）。 */
async function logout() {
  const tok = getToken();
  if (isSessionToken(tok)) {
    try { await auth.logout(); } catch { /* 切れていても消す */ }
  }
  clearToken();
  setMe(null);
  state.caps = null;
  state.password = null;
  history.replaceState(history.state, '', `${location.pathname}${location.search}#/`);
  showLogin(t('login.loggedOut'));
}

/**
 * 401 のとき: 外枠を出しているなら、画面を残したまま入り直しのダイアログ。入り直せたら true（要求を送り直す）。
 * 外枠が無い（ログイン・手続きの画面）か、ログアウトを選んだらログインの画面へ。
 */
function reauthenticate() {
  if (state.reauth) return state.reauth;
  const me = getMe();
  if (state.screen !== 'console' || !me) {
    if (state.screen === 'console') { clearToken(); toast(t('login.expiredToast'), 'warn'); showLogin(t('login.authFailed'), 'warn'); }
    return Promise.resolve(false);
  }
  closeFloating();
  state.reauth = reauthDialog({ me }).then((ok) => {
    state.reauth = null;
    if (ok) {
      // SSE は新しい値でつなぎ直す（取りこぼしは Last-Event-ID で埋まる）
      if (state.sse) { state.sse.stop(); state.sse.start(); }
      toast(t('auth.reauth.done'), 'ok', 3000);
      return true;
    }
    clearToken();
    setMe(null);
    state.caps = null;
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/`);
    showLogin(t('login.loggedOut'));
    return false;
  });
  return state.reauth;
}
setReauthHandler(reauthenticate);

// SSE の 401（要求の 401 は client.js が reauthenticate を呼ぶ）
on('unauthorized', () => {
  if (!getToken()) return;
  reauthenticate();
});

// 使っている途中で「先に済ませる手続き」が要ると言われた（管理者が仮のパスワードにした直後など）
on('pending', (pending) => {
  if (state.screen !== 'console') return;
  const me = getMe();
  if (me) showForced({ ...me, pending });
});

window.addEventListener('hashchange', () => {
  if (state.shell) route();
});

// 言語を変えたら、外枠と今の画面を作り直す（入力中の内容は消える）
onLangChange(() => {
  if (state.shell) {
    mountShell();
    route({ focus: false });
  } else if (state.screen === 'forced' && getMe()) {
    showForced(getMe());
  } else if (state.screen === 'login') {
    showLogin();
  }
});

installShortcuts({
  palette,
  navigate,
  toggleSidebar: () => state.shell && state.shell.toggleSidebar(),
  enabled: () => !!state.shell,
});

function syncStatic() {
  const skip = document.getElementById('skip-link');
  if (skip) skip.textContent = t('shell.skip');
  const toasts = document.getElementById('toasts');
  if (toasts) toasts.setAttribute('aria-label', t('shell.notifications'));
}
// 「本文へ移動」はハッシュを変えずに本文へフォーカスを移す（#main を経路と取り違えないように）
document.getElementById('skip-link')?.addEventListener('click', (e) => {
  e.preventDefault();
  const m = document.getElementById('main');
  if (m) m.focus();
});
onLangChange(syncStatic);
syncStatic();

function boot() {
  // OIDC から戻ったところ: コードを URL から消してから（履歴に残さない）替える
  const m = location.hash.match(/^#\/auth\/complete\?(.*)$/);
  if (m) {
    const params = new URLSearchParams(m[1]);
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/`);
    state.screen = 'login';
    renderAuthComplete(document.getElementById('app'), params, {
      onLogin: (r) => enter(r),
      onBack: () => showLogin(),
    });
    document.title = `${t('login.title')} — ${t('shell.title')}`;
    return;
  }
  if (!getToken()) {
    showLogin();
    return;
  }
  enter();
}

boot();
