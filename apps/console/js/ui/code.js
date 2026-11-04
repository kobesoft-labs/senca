// コードの見本（SDK の呼び方・curl など）。等幅で折り返さずに横へ流し、右上に「コピー」を置く。
//
//   codeBlock(text, { label: t('rooms.token.sdk'), lang: 'js' })

import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { copyText } from './form.js';
import { tooltip } from './overlay.js';

/**
 * @param {string} text
 * @param {{label: string, lang?: string, testid?: string}} o label はコピーの通知と読み上げの名前
 */
export function codeBlock(text, { label, lang, testid } = {}) {
  const btn = h('button', { type: 'button', class: 'ghost icon-only sm code-copy', 'aria-label': t('copy.copyLabel', { label }) }, icon('copy'));
  tooltip(btn, t('copy.copy'));
  btn.addEventListener('click', async () => {
    if (await copyText(text, label)) {
      btn.replaceChildren(icon('check'));
      setTimeout(() => btn.replaceChildren(icon('copy')), 1600);
    }
  });
  return h('div', { class: 'code-block', 'data-testid': testid },
    h('pre', { tabindex: '0', 'aria-label': label, 'data-lang': lang || null }, h('code', null, text)),
    btn);
}
