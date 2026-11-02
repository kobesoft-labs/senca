// 予約の詳細: 内容・実行履歴・終了時刻の変更・取り消し・削除。

import { h, replace, table, kv, field, select } from '../lib/dom.js';
import { load, confirmDialog, reportError, errorBox, toast } from '../lib/ui.js';
import { schedules, streams } from '../api/resources.js';
import { fmtTime, fmtTimeIn, wallToUtcIso, utcToWall, zoneChoices, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { ACTIONS, schedBadge, describeRecurrence } from './schedules.js';

export async function render(root, [id], { navigate }) {
  let sch = null;
  let etag = null;
  let streamName = null;
  const head = h('div', { class: 'page-head' });
  const info = h('div');
  const edit = h('div');
  const runs = h('div');

  const fetchSch = async () => {
    const r = await schedules.get(id);
    sch = r.data;
    etag = r.etag;
  };

  const on412 = async (e) => {
    if (e.status !== 412) return false;
    await reload();
    toast('他の操作で更新されていたので、最新の内容を読み込み直しました。もう一度操作してください', 'warn', 8000);
    return true;
  };

  const doPatch = async (body, okText) => {
    try {
      const r = await schedules.update(id, body, etag);
      sch = r.data;
      etag = r.etag;
      renderAll();
      toast(okText, 'ok');
    } catch (e) {
      if (!(await on412(e))) reportError(e, '更新');
    }
  };

  const cancel = async () => {
    if (!(await confirmDialog('予約を取り消す', 'この予約を取り消します（実行中なら止めます）。履歴は残ります。', { ok: '取り消す' }))) return;
    await doPatch({ state: 'canceled' }, '取り消しました');
  };

  const remove = async () => {
    if (!(await confirmDialog('予約を削除', 'この予約と実行履歴を削除します。作った録画は残ります。', { ok: '削除' }))) return;
    try {
      await schedules.remove(id, etag);
      toast('削除しました', 'ok');
      navigate('/schedules');
    } catch (e) {
      if (!(await on412(e))) reportError(e, '削除');
    }
  };

  const renderAll = () => {
    const tz = sch.recurrence ? sch.recurrence.timezone : null;
    const active = sch.state === 'pending' || sch.state === 'running';
    replace(head, h('h1', null, sch.name || `予約 ${sch.id.slice(0, 8)}`), schedBadge(sch.state), h('span', { class: 'spacer' }),
      active ? h('button', { onClick: cancel }, '取り消す') : null,
      h('button', { class: 'danger', onClick: remove }, '削除'));
    replace(info, kv([
      ['ID', h('code', null, sch.id)],
      ['配信', h('a', { href: `#/streams/${encodeURIComponent(sch.stream)}` }, streamName || sch.stream)],
      ['操作', ACTIONS[sch.action] || sch.action],
      ['開始', [fmtTime(sch.start_at), tz ? h('span', { class: 'muted small' }, ` / ${fmtTimeIn(sch.start_at, tz)}（${tz}）`) : null]],
      ['終了', [fmtTime(sch.end_at), tz ? h('span', { class: 'muted small' }, ` / ${fmtTimeIn(sch.end_at, tz)}（${tz}）`) : null]],
      ['準備・余白・猶予', `${sch.pre_roll_ms / 1000} 秒 / ${sch.post_roll_ms / 1000} 秒 / ${sch.grace_ms / 1000} 秒`],
      sch.action === 'snapshot' ? ['静止画の間隔', `${sch.snapshot_interval_secs} 秒`] : null,
      ['繰り返し', describeRecurrence(sch.recurrence)],
      ['録画の保持', sch.retention_days !== null ? `${sch.retention_days} 日` : '配信の設定に従う'],
      ['作成', fmtTime(sch.created_at)],
      ['世代', String(sch.generation)],
    ]));
    if (!active) {
      replace(edit, h('p', { class: 'muted' }, 'この予約は終わっているので変更できません。'));
      return;
    }
    const zone = tz || localZone();
    const zoneSel = select(zoneChoices().map((z) => [z, z]), zone);
    const name = h('input', { type: 'text', value: sch.name || '', maxlength: 128 });
    const start = h('input', { type: 'datetime-local', value: utcToWall(sch.start_at, zone), disabled: sch.state === 'running' });
    const end = h('input', { type: 'datetime-local', value: utcToWall(sch.end_at, zone) });
    zoneSel.addEventListener('change', () => {
      start.value = utcToWall(sch.start_at, zoneSel.value);
      end.value = utcToWall(sch.end_at, zoneSel.value);
    });
    replace(edit, h('div', { class: 'form' },
      field('名前', name),
      h('div', { class: 'cols' }, field('タイムゾーン（入力の解釈）', zoneSel), field('開始', start, sch.state === 'running' ? '実行中は変えられません' : null), field('終了（延長・短縮）', end)),
      h('div', { class: 'actions' }, h('button', { class: 'primary', onClick: () => {
        const body = {};
        if ((sch.name || '') !== name.value) body.name = name.value || null;
        const s = wallToUtcIso(start.value, zoneSel.value);
        const e = wallToUtcIso(end.value, zoneSel.value);
        if (sch.state !== 'running' && s && new Date(s).getTime() !== new Date(sch.start_at).getTime()) body.start_at = s;
        if (e && new Date(e).getTime() !== new Date(sch.end_at).getTime()) body.end_at = e;
        if (!Object.keys(body).length) { toast('変更はありません'); return; }
        doPatch(body, '保存しました');
      } }, '保存'))));
  };

  const loadRuns = () => load(runs, async () => {
    const { data } = await schedules.runs(id);
    return table([
      { label: '#', class: 'num', render: (r) => r.occurrence },
      { label: '開始', render: (r) => fmtTime(r.start_at) },
      { label: '終了', render: (r) => fmtTime(r.end_at) },
      { label: '結果', render: (r) => schedBadge(r.state) },
      { label: '録画', render: (r) => (r.recording ? h('a', { href: `#/recordings/${encodeURIComponent(r.recording)}` }, r.recording.slice(0, 10)) : '—') },
      { label: '理由', render: (r) => r.reason || '' },
    ], data.items.slice().reverse(), { empty: 'まだ実行していません' });
  });

  const reload = async () => {
    try {
      await fetchSch();
      renderAll();
      loadRuns();
    } catch (e) {
      if (e.status === 404) {
        toast('この予約は削除されました', 'warn');
        navigate('/schedules');
      } else reportError(e, '読み込み');
    }
  };

  try {
    await fetchSch();
  } catch (e) {
    root.append(h('h1', null, '予約'), errorBox(e), h('p', null, h('a', { href: '#/schedules' }, '一覧へ戻る')));
    return null;
  }
  try { streamName = (await streams.get(sch.stream)).data.name; } catch { /* 名前が出ないだけ */ }

  root.append(
    h('nav', { class: 'small', 'aria-label': 'パンくず' }, h('a', { href: '#/schedules' }, '予約'), ' / ', sch.name || sch.id.slice(0, 8)),
    head,
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 'sc-in' }, h('h2', { id: 'sc-in' }, '内容'), info),
      h('section', { class: 'panel', 'aria-labelledby': 'sc-ed' }, h('h2', { id: 'sc-ed' }, '変更'), edit)),
    h('section', { class: 'panel', 'aria-labelledby': 'sc-rn' }, h('h2', { id: 'sc-rn' }, '実行履歴'), runs));
  renderAll();
  loadRuns();
  return refreshOn(['schedule.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id });
}
