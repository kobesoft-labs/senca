// 配信の一覧（カーソルでページ送り）と作成。

import { h, replace, table, badge } from '../lib/dom.js';
import { load, modal, reportError, errorBox } from '../lib/ui.js';
import { streams } from '../api/resources.js';
import { fmtTime, fmtBps, idemKey } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { streamForm } from './stream-form.js';
import { stashSecret } from '../lib/secrets.js';

const PAGE = 50;

export function stateBadge(s) {
  if (s.observed_state === 'live') return badge('配信中', 'live');
  if (s.observed_state === 'stopped' || s.desired_state === 'stopped') return badge('停止', 'warn');
  return badge('入力待ち');
}

/** 作成のダイアログ。作ったら Stream を返す。 */
export function createStreamDialog(caps) {
  const form = streamForm(null, caps);
  const errBox = h('div');
  // 同じ入力の再送で二重に作らないよう、ダイアログごとに 1 つの鍵
  const key = idemKey();
  return modal('配信を作成', () => [form.el, errBox], {
    wide: true,
    actions: (close) => {
      const ok = h('button', { class: 'primary', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
        replace(errBox);
        try {
          const { data } = await streams.create(body, key);
          close(data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: '作成できませんでした' }));
        } finally {
          ok.disabled = false;
        }
      } }, '作成');
      return [h('button', { onClick: () => close(null) }, 'やめる'), ok];
    },
  });
}

export async function render(root, _params, { caps, navigate }) {
  const box = h('div');
  const pager = h('div', { class: 'pager' });
  const cursors = [undefined];
  let page = 0;

  const create = async () => {
    const s = await createStreamDialog(caps);
    if (!s) return;
    if (s.ingest && s.ingest.stream_key) stashSecret(s.id, s.ingest.stream_key);
    navigate(`/streams/${encodeURIComponent(s.id)}`);
  };

  const refresh = () => load(box, async () => {
    const { data } = await streams.list({ limit: PAGE, cursor: cursors[page] });
    cursors[page + 1] = data.next_cursor || null;
    replace(pager,
      h('button', { class: 'small', disabled: page === 0, onClick: () => { page -= 1; refresh(); } }, '← 前へ'),
      h('span', { class: 'muted small' }, `${page + 1} ページ目`),
      h('button', { class: 'small', disabled: !data.next_cursor, onClick: () => { page += 1; refresh(); } }, '次へ →'));
    return table([
      { label: '名前', render: (s) => h('a', { href: `#/streams/${encodeURIComponent(s.id)}` }, s.name) },
      { label: '状態', render: stateBadge },
      { label: '遅延', render: (s) => s.latency_profile },
      { label: 'コーデック', render: (s) => s.codec_profile },
      { label: 'HLS', render: (s) => s.hls.container },
      { label: '録画', render: (s) => (s.recording.enabled ? '自動' : '—') },
      { label: 'ビットレート', class: 'num', render: (s) => (s.live ? fmtBps(s.live.bitrate_bps) : '—') },
      { label: '更新', render: (s) => fmtTime(s.updated_at) },
    ], data.items, { empty: '配信はまだありません。「配信を作成」から作れます。', caption: '配信の一覧' });
  });

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, '配信'), h('span', { class: 'spacer' }),
      h('button', { onClick: () => refresh() }, '再読み込み'),
      h('button', { class: 'primary', onClick: () => create().catch((e) => reportError(e, '作成')) }, '＋ 配信を作成')),
    h('section', { class: 'panel' }, box, pager));
  await refresh();
  return refreshOn(['stream.'], refresh);
}
