// キーボードのショートカット。入力中（input・textarea・select・contenteditable）とダイアログを開いている間は動かない
// （Ctrl/⌘+K だけは入力中でも効く）。
//
//   Ctrl/⌘+K  コマンドパレット      ?  ショートカットの一覧      /  ページの検索（無ければパレット）
//   g → d/s/r/…  その画面へ（js/nav.js の key）     [  メニューを畳む・広げる

import { h } from './lib/dom.js';
import { t } from './i18n/index.js';
import { modal } from './ui/overlay.js';
import { NAV_GROUPS } from './nav.js';

export const IS_MAC = /Mac|iPhone|iPad|iPod/.test((globalThis.navigator && (navigator.userAgentData?.platform || navigator.platform || navigator.userAgent)) || '');
export const MOD = IS_MAC ? '⌘' : 'Ctrl';

/** 入力中か（ショートカットを効かせない）。 */
export function isTyping(el) {
  if (!el || el === document.body) return false;
  if (el.isContentEditable) return true;
  const tag = el.tagName;
  if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
  if (tag === 'INPUT') return !['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color', 'file'].includes(el.type);
  return false;
}

/**
 * @param {{palette: () => void, navigate: (href: string) => void, toggleSidebar: () => void, enabled: () => boolean}} handlers
 */
export function installShortcuts({ palette, navigate, toggleSidebar, enabled }) {
  let pendingG = 0;
  const onKey = (e) => {
    if (!enabled()) return;
    if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      palette();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (isTyping(e.target) || document.querySelector('dialog[open]') || e.defaultPrevented) return;
    const now = Date.now();
    if (pendingG && now - pendingG < 1500) {
      pendingG = 0;
      const item = NAV_GROUPS.flatMap((g) => g.items).find((x) => x.key === e.key.toLowerCase());
      if (item) {
        e.preventDefault();
        navigate(item.href);
      }
      return;
    }
    if (e.key === 'g') { pendingG = now; return; }
    if (e.key === '?') { e.preventDefault(); showShortcutHelp(); return; }
    if (e.key === '/') {
      e.preventDefault();
      const s = document.querySelector('main [data-search="page"]');
      if (s) s.focus();
      else palette();
      return;
    }
    if (e.key === '[') { e.preventDefault(); toggleSidebar(); }
  };
  document.addEventListener('keydown', onKey);
  return () => document.removeEventListener('keydown', onKey);
}

/**
 * 画面だけで効く 1 文字のショートカット（例: 一覧で N = 新規作成）。入力中・ダイアログ中は動かない。
 * 戻り値は解除する関数（画面の片付けで呼ぶ）。
 */
export function onPageKey(key, fn) {
  const l = (e) => {
    if (e.key !== key || e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    if (isTyping(e.target) || document.querySelector('dialog[open]')) return;
    e.preventDefault();
    fn();
  };
  document.addEventListener('keydown', l);
  return () => document.removeEventListener('keydown', l);
}

const keys = (...ks) => ks.map((k, i) => [i ? h('span', { class: 'then' }, t('shortcuts.then')) : null, h('kbd', null, k)]);

/** ショートカットの一覧（? で開く）。 */
export function showShortcutHelp() {
  if (document.querySelector('dialog[data-testid="shortcuts"]')) return Promise.resolve();
  const general = [
    [t('shortcuts.palette'), [h('kbd', null, MOD), h('kbd', null, 'K')]],
    [t('shortcuts.help'), keys('?')],
    [t('shortcuts.search'), keys('/')],
    [t('shortcuts.sidebar'), keys('[')],
    [t('shortcuts.listMove'), [h('kbd', null, 'J'), h('kbd', null, 'K')]],
    [t('shortcuts.listOpen'), [h('kbd', null, 'Enter')]],
    [t('shortcuts.close'), keys('Esc')],
  ];
  const go = NAV_GROUPS.flatMap((g) => g.items).filter((x) => x.key && !x.soon).map((x) => [t(`nav.${x.id}`), keys('G', x.key.toUpperCase())]);
  const section = (title, rows) => h('section', null, h('h3', null, title),
    h('dl', null, rows.flatMap(([label, ks]) => [h('dt', null, label), h('dd', null, ks)])));
  return modal(t('shortcuts.title'), () => [
    h('p', { class: 'muted small' }, t('shortcuts.lead')),
    h('div', { class: 'shortcut-list' }, section(t('shortcuts.general'), general), section(t('shortcuts.goto'), go)),
  ], { icon: 'keyboard', wide: true, testid: 'shortcuts' });
}
