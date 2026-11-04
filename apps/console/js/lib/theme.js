// 明暗の切り替え: system（OS に合わせる）・light・dark。選んだものは localStorage に残す。
// <html data-theme> には解決した light / dark を入れる（CSS は data-theme だけを見る）。

import { emit } from './bus.js';

const STORE = 'senca.console.theme';
export const THEMES = ['system', 'light', 'dark'];
const mq = globalThis.matchMedia ? globalThis.matchMedia('(prefers-color-scheme: dark)') : null;

export function getThemePref() {
  try {
    const v = localStorage.getItem(STORE);
    return THEMES.includes(v) ? v : 'system';
  } catch {
    return 'system';
  }
}

export function resolvedTheme(pref = getThemePref()) {
  if (pref === 'light' || pref === 'dark') return pref;
  return mq && mq.matches ? 'dark' : 'light';
}

function apply() {
  const pref = getThemePref();
  const root = document.documentElement;
  root.dataset.theme = resolvedTheme(pref);
  root.dataset.themePref = pref;
  emit('theme', { pref, theme: root.dataset.theme });
}

export function setThemePref(pref) {
  if (!THEMES.includes(pref)) return;
  try { localStorage.setItem(STORE, pref); } catch { /* 残せなくても切り替える */ }
  apply();
}

/** system → light → dark の順に回す（ショートカット用）。 */
export function cycleTheme() {
  const i = THEMES.indexOf(getThemePref());
  setThemePref(THEMES[(i + 1) % THEMES.length]);
}

if (mq) mq.addEventListener('change', () => { if (getThemePref() === 'system') apply(); });
apply();
