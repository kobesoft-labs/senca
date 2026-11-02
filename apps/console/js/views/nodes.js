// ノードとデバイス: 一覧・drain / undrain・コーデックのプロバイダー・変換の能力。node:admin が要る。

import { h, replace, table, badge, kv } from '../lib/dom.js';
import { load, confirmDialog, reportError, toast } from '../lib/ui.js';
import { system } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';

export async function render(root, _p, { caps }) {
  const nodesBox = h('div');
  const devBox = h('div');
  const capBox = h('div');

  const forbidden = (e) => (e.status === 403 ? h('div', { class: 'alert warn' }, 'この画面には node:admin の権限が要ります。', e.requestId ? h('div', { class: 'meta' }, `request_id: ${e.requestId}`) : null) : null);

  const act = async (n, drain) => {
    const msg = drain
      ? `「${n.name}」を drain します。新しい割り当てをしなくなります（今の配信は続きます）。`
      : `「${n.name}」の drain を解除し、新しい割り当てを受けるようにします。`;
    if (!(await confirmDialog(drain ? 'drain' : 'drain の解除', msg, { ok: drain ? 'drain' : '解除', danger: drain }))) return;
    try {
      await (drain ? system.drain(n.id) : system.undrain(n.id));
      toast(drain ? 'drain にしました' : 'drain を解除しました', 'ok');
      refresh();
    } catch (e) {
      reportError(e, drain ? 'drain' : 'drain の解除');
    }
  };

  const refreshNodes = async () => {
    try {
      const { data } = await system.nodes();
      replace(nodesBox, table([
        { label: '名前', render: (n) => [n.name, n.self ? h('span', { class: 'muted small' }, '（このノード）') : null] },
        { label: 'ID', render: (n) => h('code', null, n.id.slice(0, 12)) },
        { label: '状態', render: (n) => (n.draining ? badge('drain 中', 'warn') : badge('受付中', 'ok')) },
        { label: '配信中', class: 'num', render: (n) => n.live_streams },
        { label: 'ラベル', render: (n) => Object.entries(n.labels).map(([k, v]) => `${k}=${v}`).join(', ') || '—' },
        { label: '登録', render: (n) => fmtTime(n.registered_at) },
        { label: '操作', render: (n) => (n.draining
          ? h('button', { class: 'small', onClick: () => act(n, false) }, 'drain を解除')
          : h('button', { class: 'small danger', onClick: () => act(n, true) }, 'drain')) },
      ], data.items, { caption: 'ノードの一覧' }));
    } catch (e) {
      if (e.status !== 401) replace(nodesBox, forbidden(e) || h('div', { class: 'alert error' }, e.title));
    }
  };

  const refreshDevices = () => load(devBox, async () => {
    try {
      const { data } = await system.devices();
      return h('div', null, data.nodes.map((n) => h('div', null,
        h('h3', null, 'ノード ', h('code', null, n.node.slice(0, 12))),
        n.devices.sandbox ? kv([['サンドボックス', n.devices.sandbox]]) : h('p', { class: 'muted small' }, 'コーデック worker の情報はまだありません'),
        table([
          { label: 'プロバイダー', render: (p) => h('code', null, p.provider) },
          { label: '状態', render: (p) => (p.available ? badge('使える', 'ok') : badge('使えない', 'danger')) },
          { label: '詳細・使えない理由', render: (p) => h('span', { class: 'small break' }, p.detail) },
        ], n.devices.providers, { empty: 'コーデック worker が動いていません（--codec-workers）' }))));
    } catch (e) {
      const f = forbidden(e);
      if (f) return f;
      throw e;
    }
  });

  const renderCaps = () => {
    if (!caps) return;
    replace(capBox, table([
      { label: 'コーデック', render: (t) => h('code', null, t.codec) },
      { label: '向き', render: (t) => (t.direction === 'encode' ? '符号化' : '復号') },
      { label: 'プロバイダー', render: (t) => t.provider },
      { label: '装置', render: (t) => t.device },
      { label: 'HW', render: (t) => (t.hardware ? badge('HW', 'ok') : 'SW') },
      { label: '最大', class: 'num', render: (t) => (t.max_width ? `${t.max_width}×${t.max_height}` : '—') },
      { label: '同時', class: 'num', render: (t) => t.max_sessions ?? '—' },
    ], caps.codecs.transcode, { empty: 'コーデック worker が未接続なので、変換の能力はありません' }));
  };

  const refresh = () => Promise.all([refreshNodes(), refreshDevices()]);

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'ノードとデバイス'), h('span', { class: 'spacer' }), h('button', { onClick: refresh }, '再読み込み')),
    h('section', { class: 'panel', 'aria-labelledby': 'n-n' }, h('h2', { id: 'n-n' }, 'ノード'), nodesBox),
    h('section', { class: 'panel', 'aria-labelledby': 'n-d' }, h('h2', { id: 'n-d' }, 'デバイス'), devBox),
    h('section', { class: 'panel', 'aria-labelledby': 'n-c' }, h('h2', { id: 'n-c' }, '変換の能力（このノード）'), capBox));
  renderCaps();
  await refresh();
  return refreshOn(['node.'], refreshNodes);
}
