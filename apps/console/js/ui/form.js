// 入力: 欄（ラベル・説明・必須・その場の検査の文）・トグル・セグメント・検索・写せる欄。

import { h, nextId } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { toast } from './toast.js';
import { tooltip } from './overlay.js';

/**
 * ラベル付きの欄。input に id・aria-describedby・aria-invalid を付ける。
 * 戻り値の要素の setError(msg) で、その場の検査の文を出す・消す（null）。
 * @param {{label: string|Node, input: HTMLElement, hint?: string|Node, required?: boolean, optional?: boolean, error?: string, help?: Node, testid?: string}} o
 * @returns {HTMLElement & {setError: (msg: string|null) => void, input: HTMLElement}}
 */
export function formField({ label, input, hint, required, optional, error, help, testid }) {
  if (!input.id) input.id = nextId('f');
  const hintId = `${input.id}-hint`;
  const errId = `${input.id}-err`;
  const msg = h('div', { class: 'field-msg', id: errId, role: 'alert', hidden: true });
  if (required) input.setAttribute('aria-required', 'true');
  // 「必須」「任意」はラベルの外に置く（見た目だけ。読み上げは aria-required。ラベルの名前を変えないため）
  const lab = h('label', { for: input.id }, label);
  const marks = [
    required ? h('span', { class: 'req', 'aria-hidden': 'true' }, t('form.required')) : null,
    optional ? h('span', { class: 'opt', 'aria-hidden': 'true' }, t('form.optional')) : null,
  ].filter(Boolean);
  const el = h('div', { class: 'field', 'data-testid': testid },
    help || marks.length ? h('div', { class: 'field-label-row' }, lab, marks, help || null) : lab,
    input,
    hint ? h('div', { class: 'hint', id: hintId }, hint) : null,
    msg);
  const describe = () => {
    const ids = [hint ? hintId : null, msg.hidden ? null : errId].filter(Boolean);
    if (ids.length) input.setAttribute('aria-describedby', ids.join(' '));
    else input.removeAttribute('aria-describedby');
  };
  el.setError = (m) => {
    if (m) {
      msg.replaceChildren(icon('error'), h('span', null, m));
      msg.hidden = false;
      input.setAttribute('aria-invalid', 'true');
    } else {
      msg.replaceChildren();
      msg.hidden = true;
      input.removeAttribute('aria-invalid');
    }
    describe();
  };
  el.input = input;
  el.setError(error || null);
  return el;
}

/**
 * トグル（オン・オフ。すぐに効く設定に使う。送信が要るものはチェックボックス）。
 * @returns {{el: HTMLElement, input: HTMLInputElement}}
 */
export function toggle({ label, description, checked = false, onChange, disabled, testid }) {
  const input = h('input', { type: 'checkbox', role: 'switch', checked, disabled, 'data-testid': testid });
  if (onChange) input.addEventListener('change', () => onChange(input.checked));
  const el = h('label', { class: 'switch' }, input,
    h('span', { class: 'switch-text' }, h('span', null, label), description ? h('small', null, description) : null));
  return { el, input };
}

/**
 * セグメント（2〜5 の中から 1 つ。role="radiogroup"、←→ で移動）。
 * options: [[value, label, iconName?], …]
 * @returns {HTMLElement & {value: string, set: (v: string) => void}}
 */
export function segmented({ options, value, onChange, label, size, iconOnly, testid }) {
  const el = h('div', { class: ['segmented', size].filter(Boolean).join(' '), role: 'radiogroup', 'aria-label': label, 'data-testid': testid });
  const btns = options.map(([v, text, ic]) => {
    const b = h('button', {
      type: 'button', class: ['seg-btn', iconOnly && 'icon-only'].filter(Boolean).join(' '), role: 'radio', 'data-value': v,
      'aria-label': iconOnly ? text : null,
      onClick: () => { set(v); if (onChange) onChange(v); },
    }, ic ? icon(ic) : null, iconOnly ? null : text);
    if (iconOnly) tooltip(b, text);
    return b;
  });
  const set = (v) => {
    el.value = v;
    for (const b of btns) {
      const on = b.dataset.value === v;
      b.setAttribute('aria-checked', String(on));
      b.tabIndex = on ? 0 : -1;
    }
  };
  el.addEventListener('keydown', (e) => {
    const i = btns.indexOf(document.activeElement);
    if (i < 0) return;
    let j = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') j = (i + 1) % btns.length;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') j = (i - 1 + btns.length) % btns.length;
    if (j === null) return;
    e.preventDefault();
    btns[j].focus();
    btns[j].click();
  });
  el.append(...btns);
  el.set = set;
  set(value ?? options[0][0]);
  return el;
}

/**
 * 検索の欄（虫眼鏡・消すボタン・`/` の表示）。onInput は打つたびに呼ぶ（呼ぶ側で間引く）。
 * @returns {HTMLElement & {input: HTMLInputElement}}
 */
export function searchBox({ placeholder, value = '', onInput, label, shortcut = false, testid }) {
  const input = h('input', {
    type: 'search', value, placeholder: placeholder || t('common.search'), 'aria-label': label || placeholder || t('common.search'),
    autocomplete: 'off', spellcheck: 'false', 'data-testid': testid, 'data-search': shortcut ? 'page' : null,
  });
  const clearBtn = h('button', { type: 'button', class: 'ghost icon-only sm search-clear', 'aria-label': t('common.clear'), hidden: !value }, icon('close'));
  const sync = () => { clearBtn.hidden = !input.value; };
  input.addEventListener('input', () => { sync(); if (onInput) onInput(input.value); });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && input.value) { e.stopPropagation(); input.value = ''; sync(); if (onInput) onInput(''); }
  });
  clearBtn.addEventListener('click', () => { input.value = ''; sync(); if (onInput) onInput(''); input.focus(); });
  const el = h('div', { class: 'search' }, icon('search'), input, clearBtn, shortcut ? h('kbd', { 'aria-hidden': 'true' }, '/') : null);
  el.input = input;
  return el;
}

/** クリップボードへ写す（成功・失敗を知らせる）。 */
export async function copyText(text, label = t('copy.value')) {
  try {
    await navigator.clipboard.writeText(text);
    toast(t('copy.done', { label }), 'ok', 2500);
    return true;
  } catch {
    toast(t('copy.failed'), 'warn');
    return false;
  }
}

/**
 * 写せる欄。secret なら伏せて表示し、目のボタンで見せる・隠す。
 * @param {string} value
 * @param {string} label 読み上げの名前（「RTMP の URL」など）
 * @param {{secret?: boolean, reveal?: boolean, testid?: string}} [o] reveal=false なら secret でも見せるボタンを出さない
 */
export function copyField(value, label, { secret = false, reveal = true, testid } = {}) {
  const input = h('input', {
    type: 'text', readonly: true, value: value ?? '', 'aria-label': label, spellcheck: 'false', autocomplete: 'off',
    class: secret ? 'masked' : null, 'data-testid': testid,
  });
  input.addEventListener('focus', () => input.select());
  const copyBtn = h('button', { type: 'button', class: 'icon-only', 'aria-label': t('copy.copyLabel', { label }) }, icon('copy'));
  tooltip(copyBtn, t('copy.copy'));
  copyBtn.addEventListener('click', async () => {
    if (await copyText(value, label)) {
      copyBtn.dataset.copied = 'true';
      copyBtn.replaceChildren(icon('check'));
      setTimeout(() => { copyBtn.dataset.copied = 'false'; copyBtn.replaceChildren(icon('copy')); }, 1600);
    }
  });
  let eye = null;
  if (secret && reveal) {
    eye = h('button', { type: 'button', class: 'icon-only', 'aria-pressed': 'false', 'aria-label': t('copy.reveal', { label }) }, icon('eye'));
    tooltip(eye, () => (eye.getAttribute('aria-pressed') === 'true' ? t('copy.hide') : t('copy.show')));
    eye.addEventListener('click', () => {
      const shown = input.classList.toggle('masked') === false;
      eye.setAttribute('aria-pressed', String(shown));
      eye.replaceChildren(icon(shown ? 'eye-off' : 'eye'));
    });
  }
  return h('div', { class: ['copy-field', secret && 'is-secret'].filter(Boolean).join(' ') }, input, eye, copyBtn);
}
