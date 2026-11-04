// 浮かぶもの: ツールチップ・ダイアログ（確認・秘密の表示）・メニュー・ポップオーバー・説明の「?」。
//
// - ダイアログは <dialog> の showModal（後ろは操作できない＝フォーカスは中に閉じる）。Escape で閉じ、閉じたら開く前の要素へフォーカスを戻す。
// - メニューは role="menu"。↑↓・Home・End で移動、Escape・Tab・外のクリックで閉じてボタンへ戻る。

import { h, nextId } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { guideUrl } from '../lib/guide.js';

// ---------------------------------------------------------------- ツールチップ
let tipEl = null;
let tipFor = null;

function placeNear(el, anchor, { gap = 8, align = 'center', prefer = 'below' } = {}) {
  const r = anchor.getBoundingClientRect();
  const w = el.offsetWidth;
  const hgt = el.offsetHeight;
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  let top = prefer === 'above' ? r.top - hgt - gap : r.bottom + gap;
  if (prefer !== 'above' && top + hgt > vh - 8) top = r.top - hgt - gap;
  if (prefer === 'above' && top < 8) top = r.bottom + gap;
  let left = align === 'start' ? r.left : align === 'end' ? r.right - w : r.left + r.width / 2 - w / 2;
  left = Math.max(8, Math.min(left, vw - w - 8));
  top = Math.max(8, Math.min(top, vh - hgt - 8));
  el.style.left = `${Math.round(left)}px`;
  el.style.top = `${Math.round(top)}px`;
}

/** 浮かぶ要素を最前面（top layer）に出す。ダイアログの中のボタンなら、そのダイアログの中に置く（外は操作できないため）。 */
function float(el, anchor) {
  el.setAttribute('popover', 'manual');
  (anchor.closest('dialog') || document.body).appendChild(el);
  if (typeof el.showPopover === 'function') el.showPopover();
}

function hideTip() {
  if (tipEl) tipEl.remove();
  if (tipFor) tipFor.removeAttribute('aria-describedby');
  tipEl = null;
  tipFor = null;
}

/**
 * ホバー・フォーカスで短い説明を出す。text は関数でもよい（出すときに引く）。
 * アイコンだけのボタンでは aria-label と同じ文になるので、読み上げの重複を避けて aria-describedby は付けない。
 */
export function tooltip(el, text, { kbd } = {}) {
  let timer = null;
  const show = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (!el.isConnected) return;
      hideTip();
      const content = typeof text === 'function' ? text() : text;
      if (!content) return;
      tipEl = h('div', { class: 'tooltip', role: 'tooltip', id: nextId('tip') }, content, kbd ? h('kbd', null, kbd) : null);
      float(tipEl, el);
      placeNear(tipEl, el, { prefer: 'below' });
      tipFor = el;
      if (el.getAttribute('aria-label') !== content) el.setAttribute('aria-describedby', tipEl.id);
    }, 350);
  };
  const hide = () => { clearTimeout(timer); if (tipFor === el) hideTip(); };
  el.addEventListener('mouseenter', show);
  el.addEventListener('focus', (e) => { if (e.target.matches(':focus-visible')) show(); });
  el.addEventListener('mouseleave', hide);
  el.addEventListener('blur', hide);
  el.addEventListener('click', hide);
  el.addEventListener('keydown', (e) => { if (e.key === 'Escape') hide(); });
  return el;
}

// ---------------------------------------------------------------- ダイアログ

/** ダイアログの中で Tab・Shift+Tab を回す（外へ出さない）。 */
export function trapFocus(dlg) {
  dlg.addEventListener('keydown', (e) => {
    if (e.key !== 'Tab') return;
    const items = [...dlg.querySelectorAll('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')]
      .filter((el) => !el.disabled && el.offsetParent !== null && !el.closest('[popover]'));
    if (!items.length) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && (document.activeElement === first || !dlg.contains(document.activeElement))) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && (document.activeElement === last || !dlg.contains(document.activeElement))) { e.preventDefault(); first.focus(); }
  });
}

/** 開いた要素がもう無ければ（描き直された）、同じ testid か同じ名前のボタンを探す。 */
function restoreFocus(opener) {
  if (!opener) return;
  let el = opener.isConnected ? opener : null;
  if (!el && opener.dataset && opener.dataset.testid) el = document.querySelector(`[data-testid="${CSS.escape(opener.dataset.testid)}"]`);
  if (!el && opener.tagName === 'BUTTON') {
    const name = opener.getAttribute('aria-label') || opener.textContent;
    el = [...document.querySelectorAll('main button')].find((b) => (b.getAttribute('aria-label') || b.textContent) === name) || null;
  }
  if (el && typeof el.focus === 'function') el.focus();
}

/**
 * モーダルのダイアログ。戻り値の Promise は閉じたときの値で解決する（Escape・×は undefined）。
 * build(close) は本文の要素を返す。actions(close) は下のボタンを返す。
 * @param {string} title
 * @param {(close: (v?: any) => void) => Node|Node[]} build
 * @param {{actions?: (close) => Node[], wide?: boolean, icon?: string, kind?: 'danger'|'warn'|'info', testid?: string, initialFocus?: string}} [o]
 */
export function modal(title, build, { actions, wide, icon: iconName, kind, testid, initialFocus } = {}) {
  return new Promise((resolve) => {
    const titleId = nextId('dlg-title');
    const dlg = h('dialog', { 'aria-labelledby': titleId, class: wide ? 'wide' : null, 'data-testid': testid });
    const opener = document.activeElement;
    let done = false;
    const close = (v) => {
      if (done) return;
      done = true;
      dlg.close();
      dlg.remove();
      restoreFocus(opener);
      resolve(v);
    };
    dlg.addEventListener('cancel', (e) => { e.preventDefault(); close(undefined); });
    // 背景（::backdrop）のクリックで閉じる
    dlg.addEventListener('mousedown', (e) => {
      if (e.target !== dlg) return;
      const r = dlg.getBoundingClientRect();
      if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) close(undefined);
    });
    const body = build(close);
    dlg.append(
      h('div', { class: 'dlg-head' },
        iconName ? h('div', { class: ['dlg-icon', kind] }, icon(iconName)) : null,
        h('h2', { id: titleId }, title),
        h('button', { type: 'button', class: 'ghost icon-only sm dlg-close', 'aria-label': t('common.close'), onClick: () => close(undefined) }, icon('close'))),
      h('div', { class: 'dlg-body' }, body),
      actions ? h('div', { class: 'dlg-foot' }, actions(close)) : null,
    );
    document.body.appendChild(dlg);
    dlg.showModal();
    trapFocus(dlg);
    const first = (initialFocus && dlg.querySelector(initialFocus))
      || dlg.querySelector('.dlg-body [autofocus], .dlg-body input:not([type=hidden]):not([readonly]), .dlg-body select, .dlg-body textarea')
      || dlg.querySelector('.dlg-foot button.primary, .dlg-foot button:not(.danger)')
      || dlg.querySelector('button');
    if (first) first.focus();
  });
}

/**
 * 確認のダイアログ。message には何が起きるか（結果）を書く。consequences で箇条書きを足せる。
 * requireText を渡すと、その文字を入れるまで実行できない（取り消せない削除に使う）。
 * @returns {Promise<boolean>}
 */
export function confirmDialog(title, message, { ok, cancel, danger = true, requireText, consequences, icon: iconName, testid } = {}) {
  let input = null;
  let okBtn = null;
  const sync = () => { if (okBtn && requireText) okBtn.disabled = input.value !== requireText; };
  return modal(title, () => {
    const parts = [h('div', { class: 'consequence' },
      h('p', null, message),
      consequences && consequences.length ? h('ul', null, consequences.map((c) => h('li', null, c))) : null)];
    if (requireText) {
      input = h('input', { type: 'text', autocomplete: 'off', spellcheck: 'false', 'aria-label': t('confirm.typeLabel', { text: requireText }) });
      input.addEventListener('input', sync);
      parts.push(h('div', { class: 'type-confirm' },
        h('label', { class: 'small' }, t('confirm.typePrefix'), ' ', h('code', null, requireText), ' ', t('confirm.typeSuffix')),
        input));
    }
    return parts;
  }, {
    icon: iconName || (danger ? 'warning' : 'info'),
    kind: danger ? 'danger' : 'info',
    testid,
    actions: (close) => {
      okBtn = h('button', { type: 'button', class: danger ? 'danger solid' : 'primary', 'data-testid': 'confirm-ok', onClick: () => {
        if (requireText && input.value !== requireText) {
          input.focus();
          return;
        }
        close(true);
      } }, ok || t('common.run'));
      sync();
      return [h('button', { type: 'button', onClick: () => close(false) }, cancel || t('common.cancel')), okBtn];
    },
  }).then((v) => !!v);
}

// ---------------------------------------------------------------- メニュー
let openMenu = null;

function closeMenu(focusBack = true) {
  if (!openMenu) return;
  const { el, anchor, off } = openMenu;
  openMenu = null;
  off();
  el.remove();
  anchor.setAttribute('aria-expanded', 'false');
  if (focusBack) anchor.focus();
}

/**
 * ボタンにメニューを付ける。items は開くたびに呼ぶ関数でもよい。
 * item: {label, icon, onSelect, danger, checked (true/false で menuitemradio), kbd, disabled, testid}
 *     | {separator: true} | {heading: '…'} | {node: Element}（情報の欄）
 */
export function attachMenu(anchor, items, { align = 'end', label } = {}) {
  const id = nextId('menu');
  anchor.setAttribute('aria-haspopup', 'menu');
  anchor.setAttribute('aria-expanded', 'false');
  anchor.setAttribute('aria-controls', id);
  const open = async (focusLast = false) => {
    if (openMenu && openMenu.anchor === anchor) { closeMenu(); return; }
    closeMenu(false);
    const list = typeof items === 'function' ? await items() : items;
    const el = h('div', { class: 'menu', role: 'menu', id, 'aria-label': label || anchor.getAttribute('aria-label') || anchor.textContent });
    const entries = [];
    for (const it of list) {
      if (!it) continue;
      if (it.separator) { el.appendChild(h('div', { class: 'menu-sep', role: 'separator' })); continue; }
      if (it.heading) { el.appendChild(h('div', { class: 'menu-label', role: 'presentation' }, it.heading)); continue; }
      if (it.node) { el.appendChild(h('div', { class: 'menu-info', role: 'presentation' }, it.node)); continue; }
      const radio = typeof it.checked === 'boolean';
      const b = h('button', {
        type: 'button', role: radio ? 'menuitemradio' : 'menuitem', tabindex: '-1', class: it.danger ? 'danger' : null,
        'aria-checked': radio ? String(it.checked) : null, disabled: it.disabled, 'data-testid': it.testid,
        onClick: () => { closeMenu(); if (it.onSelect) it.onSelect(); },
      }, it.icon ? icon(it.icon) : null, h('span', null, it.label), it.kbd ? h('kbd', null, it.kbd) : null,
      radio ? icon('check', { class: 'menu-check' }) : null);
      entries.push(b);
      el.appendChild(b);
    }
    float(el, anchor);
    placeNear(el, anchor, { align, gap: 6 });
    anchor.setAttribute('aria-expanded', 'true');
    const move = (i) => { const n = entries.length; if (!n) return; entries[((i % n) + n) % n].focus(); };
    el.addEventListener('keydown', (e) => {
      const i = entries.indexOf(document.activeElement);
      if (e.key === 'ArrowDown') { e.preventDefault(); move(i + 1); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); move(i - 1); }
      else if (e.key === 'Home') { e.preventDefault(); move(0); }
      else if (e.key === 'End') { e.preventDefault(); move(entries.length - 1); }
      else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closeMenu(); }
      else if (e.key === 'Tab') { closeMenu(false); }
    });
    const onDoc = (e) => { if (!el.contains(e.target) && !anchor.contains(e.target)) closeMenu(false); };
    const onScroll = (e) => { if (!el.contains(e.target)) closeMenu(false); };
    setTimeout(() => document.addEventListener('mousedown', onDoc), 0);
    window.addEventListener('resize', onScroll);
    document.addEventListener('scroll', onScroll, true);
    openMenu = { el, anchor, off: () => { document.removeEventListener('mousedown', onDoc); window.removeEventListener('resize', onScroll); document.removeEventListener('scroll', onScroll, true); } };
    move(focusLast ? entries.length - 1 : Math.max(0, entries.findIndex((b) => b.getAttribute('aria-checked') === 'true')));
  };
  anchor.addEventListener('click', () => open());
  anchor.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); open(); }
    if (e.key === 'ArrowUp') { e.preventDefault(); open(true); }
  });
  return anchor;
}

// ---------------------------------------------------------------- ポップオーバー
let openPop = null;

function closePopover(focusBack = true) {
  if (!openPop) return;
  const { el, anchor, off } = openPop;
  openPop = null;
  off();
  el.remove();
  anchor.setAttribute('aria-expanded', 'false');
  if (focusBack) anchor.focus();
}

/** ボタンの近くに説明の箱を出す（role="dialog"、非モーダル）。content は要素か、要素を返す関数。 */
export function attachPopover(anchor, content, { label } = {}) {
  const id = nextId('pop');
  anchor.setAttribute('aria-expanded', 'false');
  anchor.setAttribute('aria-controls', id);
  anchor.addEventListener('click', () => {
    if (openPop && openPop.anchor === anchor) { closePopover(); return; }
    closePopover(false);
    const el = h('div', { class: 'popover', role: 'dialog', id, 'aria-label': label || anchor.getAttribute('aria-label'), tabindex: '-1' },
      typeof content === 'function' ? content() : content);
    float(el, anchor);
    placeNear(el, anchor, { align: 'start', gap: 6 });
    anchor.setAttribute('aria-expanded', 'true');
    const onKey = (e) => { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closePopover(); } };
    const onDoc = (e) => { if (!el.contains(e.target) && !anchor.contains(e.target)) closePopover(false); };
    el.addEventListener('keydown', onKey);
    anchor.addEventListener('keydown', onKey);
    setTimeout(() => document.addEventListener('mousedown', onDoc), 0);
    openPop = { el, anchor, off: () => { document.removeEventListener('mousedown', onDoc); anchor.removeEventListener('keydown', onKey); } };
    el.focus();
  });
  return anchor;
}

/**
 * 「?」の説明。はじめての人向けに、短い説明と手引き（docs/guide）へのリンクを出す。
 * @param {{title: string, body: string|Node, guide?: string}} o guide は 'ja/05-recording.md#…' の言語を除いた '05-recording.md#…'
 */
export function helpHint({ title, body, guide }) {
  const b = h('button', { type: 'button', class: 'help-hint', 'aria-label': t('help.about', { topic: title }) }, icon('help'));
  attachPopover(b, () => [
    h('h3', null, title),
    h('p', null, body),
    guide ? h('a', { href: guideUrl(guide), target: '_blank', rel: 'noopener noreferrer', class: 'row small' }, icon('book', { size: 'sm' }), t('help.readGuide'), icon('external', { size: 'sm' })) : null,
  ], { label: title });
  return b;
}

/** 開いているメニュー・ポップオーバーを閉じる（画面の切り替えで使う）。 */
export function closeFloating() {
  closeMenu(false);
  closePopover(false);
  hideTip();
}
