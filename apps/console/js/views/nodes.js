// ノードとデバイス: 一覧・drain / undrain・コーデックのプロバイダー・変換の能力。node:admin が要る。

import { h, replace, kv } from '../lib/dom.js';
import { load, confirmDialog, reportError, toast } from '../lib/ui.js';
import { system } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t } from '../i18n/index.js';
import { button, iconButton, pageHeader, panel, dataTable, statusPill, badge, alert, helpHint } from '../ui/index.js';

export async function render(root, _p, { caps }) {
  const nodesBox = h('div');
  const devBox = h('div');
  const capBox = h('div');

  const forbidden = (e) => (e.status === 403
    ? alert('warn', [t('nodes.forbidden'), e.requestId ? h('div', { class: 'meta' }, `request_id: ${e.requestId}`) : null])
    : null);

  const act = async (n, drain) => {
    const ok = await confirmDialog(drain ? t('nodes.drainTitle') : t('nodes.undrainTitle'),
      drain ? t('nodes.drainBody', { name: n.name }) : t('nodes.undrainBody', { name: n.name }), {
        ok: drain ? 'drain' : t('nodes.undrainOk'),
        danger: drain,
        consequences: drain ? [t('nodes.drainC1'), t('nodes.drainC2')] : undefined,
      });
    if (!ok) return;
    try {
      await (drain ? system.drain(n.id) : system.undrain(n.id));
      toast(drain ? t('nodes.drained') : t('nodes.undrained'), 'ok');
      refresh();
    } catch (e) {
      reportError(e, drain ? 'drain' : t('nodes.undrainTitle'));
    }
  };

  const refreshNodes = async () => {
    try {
      const { data } = await system.nodes();
      replace(nodesBox, dataTable({
        caption: t('nodes.caption'),
        sort: { key: 'name', dir: 'asc' },
        rowKey: (n) => n.id,
        columns: [
          { key: 'name', label: t('common.name'), primary: true, sort: (n) => n.name, render: (n) => [n.name, n.self ? h('span', { class: 'muted small' }, t('common.thisNode')) : null] },
          { label: 'ID', render: (n) => h('code', null, n.id.slice(0, 12)) },
          { key: 'state', label: t('common.state'), sort: (n) => (n.draining ? 1 : 0), render: (n) => statusPill(n.draining ? 'draining' : 'accepting') },
          { key: 'live', label: t('nodes.col.live'), class: 'num', sort: (n) => n.live_streams, render: (n) => n.live_streams },
          { label: t('nodes.col.labels'), wrap: true, render: (n) => Object.entries(n.labels).map(([k, v]) => `${k}=${v}`).join(', ') || '—' },
          { key: 'reg', label: t('nodes.col.registered'), sort: (n) => n.registered_at, render: (n) => fmtTime(n.registered_at) },
          { label: t('common.actions'), actions: true, render: (n) => (n.draining
            ? button(t('nodes.undrain'), { size: 'sm', icon: 'play', onClick: () => act(n, false) })
            : button('drain', { size: 'sm', variant: 'danger', icon: 'pause', onClick: () => act(n, true) })) },
        ],
        rows: data.items,
      }));
    } catch (e) {
      if (e.status !== 401) replace(nodesBox, forbidden(e) || alert('error', e.title));
    }
  };

  const refreshDevices = () => load(devBox, async () => {
    try {
      const { data } = await system.devices();
      return h('div', null, data.nodes.map((n) => h('div', null,
        h('h3', null, t('nodes.nodeHeading'), ' ', h('code', null, n.node.slice(0, 12))),
        n.devices.sandbox ? kv([[t('nodes.sandbox'), n.devices.sandbox]]) : h('p', { class: 'muted small' }, t('nodes.noWorkerInfo')),
        h('div', { class: 'mt-2' }),
        dataTable({
          columns: [
            { label: t('nodes.col.provider'), primary: true, render: (p) => h('code', null, p.provider) },
            { label: t('common.state'), render: (p) => statusPill(p.available ? 'available' : 'unavailable') },
            { label: t('nodes.col.detail'), wrap: true, render: (p) => h('span', { class: 'small break' }, p.detail) },
          ],
          rows: n.devices.providers,
          empty: { icon: 'gpu', title: t('nodes.noWorker'), body: t('nodes.noWorkerBody') },
        }))));
    } catch (e) {
      const f = forbidden(e);
      if (f) return f;
      throw e;
    }
  }, { kind: 'table' });

  const renderCaps = () => {
    if (!caps) return;
    replace(capBox, dataTable({
      sort: { key: 'codec', dir: 'asc' },
      columns: [
        { key: 'codec', label: t('nodes.col.codec'), primary: true, sort: (x) => x.codec, render: (x) => h('code', null, x.codec) },
        { key: 'dir', label: t('nodes.col.direction'), sort: (x) => x.direction, render: (x) => (x.direction === 'encode' ? t('nodes.encode') : t('nodes.decode')) },
        { label: t('nodes.col.provider'), render: (x) => x.provider },
        { label: t('nodes.col.device'), render: (x) => x.device },
        { label: 'HW', render: (x) => (x.hardware ? badge('HW', 'ok') : 'SW') },
        { label: t('nodes.col.max'), class: 'num', render: (x) => (x.max_width ? `${x.max_width}×${x.max_height}` : '—') },
        { label: t('nodes.col.sessions'), class: 'num', render: (x) => x.max_sessions ?? '—' },
      ],
      rows: caps.codecs.transcode,
      empty: { icon: 'cpu', title: t('nodes.noCaps'), body: t('nodes.noWorkerBody') },
    }));
  };

  const refresh = () => Promise.all([refreshNodes(), refreshDevices()]);

  root.append(
    pageHeader({
      title: t('nav.nodes'),
      description: t('nodes.lead'),
      help: helpHint({ title: 'drain', body: t('nodes.drainHelp'), guide: '09-cluster-grid.md' }),
      actions: [iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() })],
    }),
    panel({ title: t('nodes.nodes'), icon: 'nodes' }, nodesBox),
    panel({ title: t('nodes.devices'), icon: 'gpu', description: t('nodes.devicesDesc') }, devBox),
    panel({ title: t('nodes.caps'), icon: 'zap' }, capBox));
  renderCaps();
  await refresh();
  return refreshOn(['node.'], refreshNodes);
}
