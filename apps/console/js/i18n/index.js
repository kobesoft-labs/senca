// 画面の言葉（日本語・英語）。t('streams.title') のように鍵で引く。
//
// - 辞書は js/i18n/ja/*.js と js/i18n/en/*.js（画面ごとのファイル）。両方に同じ鍵を置く（tools/interop/console.sh の C で確かめる）。
// - {name} は params で置き換える。数で言い方が変わるもの（英語の単数・複数）は tn(key, n) で `.one`・`.other` を選ぶ。
// - 既定は navigator.language（ja で始まれば日本語、ほかは英語）。切り替えは localStorage に残す。
// - 辞書に無い鍵は鍵そのものを出し、missingKeys() に溜める（ブラウザの試験で空であることを確かめる）。

import ja from './ja/index.js';
import en from './en/index.js';

export const DICTS = { ja, en };
export const LANGS = [['ja', '日本語'], ['en', 'English']];
const STORE = 'senca.console.lang';
const listeners = new Set();
const missing = new Set();

/** navigator.language などから言語を決める（ja で始まれば ja、ほかは en）。 */
export function detectLang(navLang) {
  return /^ja\b/i.test(String(navLang || '')) ? 'ja' : 'en';
}

function stored() {
  try { return globalThis.localStorage ? globalThis.localStorage.getItem(STORE) : null; } catch { return null; }
}

let lang = (() => {
  const s = stored();
  if (s && DICTS[s]) return s;
  return detectLang(globalThis.navigator ? globalThis.navigator.language : 'en');
})();

export function getLang() {
  return lang;
}

/** Intl に渡す地域（ja-JP・en-US）。 */
export function locale() {
  return lang === 'ja' ? 'ja-JP' : 'en-US';
}

/** 言語を変える。persist=false なら残さない（試験用）。 */
export function setLang(next, { persist = true } = {}) {
  if (!DICTS[next] || next === lang) return;
  lang = next;
  if (persist) {
    try { globalThis.localStorage.setItem(STORE, next); } catch { /* 残せなくても切り替える */ }
  }
  if (globalThis.document) globalThis.document.documentElement.lang = next;
  for (const fn of listeners) fn(next);
}

/** 言語の切り替えを購読する。戻り値は解除する関数。 */
export function onLangChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function interpolate(s, params) {
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, k) => (params[k] === undefined || params[k] === null ? m : String(params[k])));
}

/** 鍵の文を引く。params の {name} を置き換える。 */
export function t(key, params) {
  const d = DICTS[lang];
  let s = d[key];
  if (s === undefined) {
    s = DICTS.ja[key] ?? DICTS.en[key];
    missing.add(`${lang}:${key}`);
    if (s === undefined) return key;
  }
  return interpolate(s, params);
}

/** 数で言い方を変える（key.one・key.other。日本語は other だけでよい）。{n} に数が入る。 */
export function tn(key, n, params = {}) {
  const rule = new Intl.PluralRules(locale()).select(n);
  const k = DICTS[lang][`${key}.${rule}`] !== undefined ? `${key}.${rule}` : `${key}.other`;
  return t(k, { n: fmtInt(n), ...params });
}

function fmtInt(n) {
  try { return new Intl.NumberFormat(locale()).format(n); } catch { return String(n); }
}

/** 鍵があるか（任意の文言を出し分けるとき）。 */
export function has(key) {
  return DICTS[lang][key] !== undefined;
}

/** 実行中に見つからなかった鍵（`lang:key`）。 */
export function missingKeys() {
  return [...missing];
}

if (globalThis.document) globalThis.document.documentElement.lang = lang;
if (globalThis.window) globalThis.window.__sencaI18nMissing = missingKeys;
