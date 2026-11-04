// ボタン。種類（variant）は primary・secondary（既定）・ghost・danger（赤い文字）・danger-solid（赤い塗り）・link。
// 1 画面で primary は 1 つまで。赤は取り消せない操作にだけ使う。

import { h } from '../lib/dom.js';
import { icon as iconEl } from '../lib/icons.js';
import { tooltip } from './overlay.js';

const VARIANT = {
  primary: 'primary', secondary: null, ghost: 'ghost', danger: 'danger', 'danger-solid': 'danger solid', link: 'link',
};

/**
 * @param {string} label 文字（iconOnly なら aria-label とツールチップ）
 * @param {object} [o]
 * @param {'primary'|'secondary'|'ghost'|'danger'|'danger-solid'|'link'} [o.variant]
 * @param {'sm'|'md'|'lg'} [o.size]
 * @param {string} [o.icon] 左のアイコン
 * @param {string} [o.iconRight] 右のアイコン
 * @param {boolean} [o.iconOnly] アイコンだけ（label は読み上げとツールチップ）
 * @param {string} [o.kbd] ツールチップに出すキー（例 'N'）
 * @param {(e: MouseEvent) => any} [o.onClick] Promise を返すと終わるまで読み込み中にする
 * @param {string} [o.testid] data-testid
 * @param {object} [o.attrs] そのほかの属性（disabled・type・title など）
 */
export function button(label, o = {}) {
  const cls = ['btn', VARIANT[o.variant || 'secondary'], o.size === 'sm' ? 'sm' : o.size === 'lg' ? 'lg' : null, o.iconOnly ? 'icon-only' : null, o.class];
  const el = h('button', {
    type: 'button',
    class: cls.filter(Boolean).join(' '),
    'aria-label': o.iconOnly ? label : null,
    'data-testid': o.testid,
    ...o.attrs,
  },
  o.icon ? iconEl(o.icon) : null,
  o.iconOnly ? null : h('span', { class: 'btn-label' }, label),
  o.iconRight ? iconEl(o.iconRight) : null);
  if (o.iconOnly) tooltip(el, label, { kbd: o.kbd });
  else if (o.kbd) tooltip(el, label, { kbd: o.kbd });
  if (o.onClick) {
    el.addEventListener('click', async (e) => {
      const r = o.onClick(e);
      if (r && typeof r.then === 'function') {
        setLoading(el, true);
        try { await r; } finally { setLoading(el, false); }
      }
    });
  }
  return el;
}

/** アイコンだけのボタン（label は必須: 読み上げとツールチップに使う）。 */
export function iconButton(name, label, o = {}) {
  return button(label, { variant: 'ghost', ...o, icon: name, iconOnly: true });
}

/** 読み込み中（押せなくして、先頭のアイコンを回す。アイコンが無ければ足す）。 */
export function setLoading(btn, on) {
  if (on) {
    btn.dataset.wasDisabled = btn.disabled ? '1' : '';
    btn.disabled = true;
    btn.setAttribute('aria-busy', 'true');
    const first = btn.querySelector('svg.icon');
    if (first) {
      btn._icon = first;
      first.replaceWith(iconEl('spinner'));
    } else {
      btn.prepend(iconEl('spinner'));
      btn._icon = null;
    }
  } else {
    btn.removeAttribute('aria-busy');
    btn.disabled = btn.dataset.wasDisabled === '1';
    const sp = btn.querySelector('svg.icon[data-icon="spinner"]');
    if (sp) {
      if (btn._icon) sp.replaceWith(btn._icon);
      else sp.remove();
    }
  }
}

/** ボタンを横に並べる。 */
export function buttonGroup(...buttons) {
  return h('div', { class: 'btn-group' }, buttons);
}
