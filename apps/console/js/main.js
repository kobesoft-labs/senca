// SENCA 管理画面の入口。ハッシュの経路（#/streams など）で画面を切り替える。
// 使う API は公開の /api/v1 だけ（REQ-002）。トークンは sessionStorage にだけ置く。
//
// 画面のモジュールは render(main, params, ctx) を持ち、片付けの関数を返してよい。
// ctx = { caps, navigate(path), setCrumbs(items) }。経路の一覧は js/nav.js。

import { h, replace } from './lib/dom.js';
import { on } from './lib/bus.js';
import { t, onLangChange } from './i18n/index.js';
import './lib/theme.js';
import { toast } from './ui/toast.js';
import { closeFloating } from './ui/overlay.js';
import { skeleton } from './ui/feedback.js';
import { getToken, clearToken } from './api/client.js';
import { system } from './api/resources.js';
import { EventStream } from './api/sse.js';
import { renderLogin } from './views/login.js';
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

function showLogin(message = state.loginMessage) {
  runCleanup();
  if (state.sse) state.sse.stop();
  state.sse = null;
  if (state.shell) state.shell.destroy();
  state.shell = null;
  state.loginMessage = message || null;
  closeFloating();
  renderLogin(document.getElementById('app'), { message, onLogin: (caps) => { state.caps = caps; state.loginMessage = null; startSession(); } });
  document.title = `${t('login.title')} — ${t('shell.title')}`;
}

function startSession() {
  state.sse = new EventStream();
  mountShell();
  state.sse.start();
  route({ focus: false });
}

function logout() {
  clearToken();
  state.caps = null;
  showLogin(t('login.loggedOut'));
}

on('unauthorized', () => {
  if (!getToken()) return;
  clearToken();
  state.caps = null;
  toast(t('login.expiredToast'), 'warn');
  showLogin(t('login.authFailed'));
});

window.addEventListener('hashchange', () => {
  if (state.shell) route();
});

// 言語を変えたら、外枠と今の画面を作り直す（入力中の内容は消える）
onLangChange(() => {
  if (state.shell) {
    mountShell();
    route({ focus: false });
  } else if (!getToken()) {
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

async function boot() {
  if (!getToken()) {
    showLogin();
    return;
  }
  try {
    const { data } = await system.capabilities({ quiet401: true });
    state.caps = data;
    startSession();
  } catch (e) {
    if (e.status === 401) {
      clearToken();
      showLogin(t('login.storedInvalid'));
    } else {
      showLogin(t('login.cannotConnect', { reason: e.title || e.message }));
    }
  }
}

boot();
