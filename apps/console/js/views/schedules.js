// 予約の一覧と作成（作成前に POST /schedules/check で重なりを確かめる）。

import { h, replace, table, badge, select, field, checkbox } from '../lib/dom.js';
import { load, modal, reportError, errorBox } from '../lib/ui.js';
import { schedules, streams, listAll } from '../api/resources.js';
import { fmtTime, fmtTimeIn, fmtRelative, wallToUtcIso, utcToWall, zoneChoices, localZone, WEEKDAYS, idemKey } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';

const PAGE = 50;

export const ACTIONS = { record: '録画', live: '配信の受付', snapshot: '静止画' };

export function schedBadge(state) {
  const kind = { running: 'live', pending: '', completed: 'ok', missed: 'warn', canceled: 'warn', failed: 'danger' }[state];
  const text = { running: '実行中', pending: '待機', completed: '完了', missed: '逃した', canceled: '取消', failed: '失敗' }[state] || state;
  return badge(text, kind);
}

function describeRecurrence(r) {
  if (!r) return '1 回';
  const days = r.freq === 'weekly' && r.weekdays && r.weekdays.length ? `（${r.weekdays.map((d) => WEEKDAYS[d]).join('・')}）` : '';
  const end = r.count ? `・${r.count} 回` : r.until_ms ? `・${fmtTime(new Date(r.until_ms).toISOString())} まで` : '';
  return `${r.freq === 'daily' ? '毎日' : '毎週'}${days}・${r.timezone}${end}`;
}

export { describeRecurrence };

function scheduleForm(streamList, preset) {
  const zone = localZone();
  const now = Date.now();
  const startDefault = Math.ceil((now + 5 * 60_000) / 300_000) * 300_000;
  const stream = select(streamList.map((s) => [s.id, s.name]), preset || (streamList[0] && streamList[0].id));
  const name = h('input', { type: 'text', maxlength: 128, placeholder: '例: 朝の講義' });
  const action = select(Object.entries(ACTIONS), 'record');
  const tz = select(zoneChoices().map((z) => [z, z === zone ? `${z}（この端末）` : z]), zone);
  const start = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault, zone) });
  const end = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault + 3600_000, zone) });
  const pre = h('input', { type: 'number', min: 0, max: 300, value: 30 });
  const post = h('input', { type: 'number', min: 0, max: 300, value: 0 });
  const grace = h('input', { type: 'number', min: 0, max: 3600, value: 60 });
  const snapInt = h('input', { type: 'number', min: 1, max: 3600, value: 60 });
  const retention = h('input', { type: 'number', min: 0, placeholder: '配信の設定に従う' });
  const rep = select([['none', '繰り返さない'], ['daily', '毎日'], ['weekly', '毎週']], 'none');
  const wd = WEEKDAYS.map((d, i) => checkbox(d, false, { value: String(i) }));
  const repEndKind = select([['none', '終わりなし'], ['count', '回数'], ['until', '日付まで']], 'none');
  const count = h('input', { type: 'number', min: 1, value: 10 });
  const until = h('input', { type: 'date' });
  const utcPreview = h('div', { class: 'hint', 'aria-live': 'polite' });

  const wdBox = h('div', { class: 'inline', role: 'group', 'aria-label': '曜日' }, wd.map((x) => x.el));
  const repEndBox = h('div', { class: 'inline' }, repEndKind, count, until);
  const snapField = field('静止画の間隔（秒）', snapInt);

  const sync = () => {
    wdBox.hidden = rep.value !== 'weekly';
    repEndBox.hidden = rep.value === 'none';
    count.hidden = repEndKind.value !== 'count';
    until.hidden = repEndKind.value !== 'until';
    snapField.hidden = action.value !== 'snapshot';
    const s = wallToUtcIso(start.value, tz.value);
    const e = wallToUtcIso(end.value, tz.value);
    utcPreview.textContent = s && e ? `UTC: ${s} 〜 ${e}${tz.value !== zone ? `（この端末の時刻: ${fmtTime(s)} 〜 ${fmtTime(e)}）` : ''}` : '';
  };
  for (const el of [rep, repEndKind, action, start, end, tz]) el.addEventListener('input', sync);
  for (const el of [rep, repEndKind, action, tz]) el.addEventListener('change', sync);
  // 開始を動かしたら終わりも同じだけ動かす
  let lastStart = start.value;
  start.addEventListener('change', () => {
    const a = wallToUtcIso(lastStart, tz.value);
    const b = wallToUtcIso(start.value, tz.value);
    const e = wallToUtcIso(end.value, tz.value);
    if (a && b && e) end.value = utcToWall(new Date(e).getTime() + (new Date(b) - new Date(a)), tz.value);
    lastStart = start.value;
    sync();
  });
  sync();

  const errors = h('div', { role: 'alert' });
  const el = h('div', { class: 'form' },
    h('div', { class: 'cols' }, field('配信', stream), field('操作', action), field('名前（任意）', name)),
    h('fieldset', null, h('legend', null, '時刻'),
      h('div', { class: 'cols' }, field('タイムゾーン', tz), field('開始', start), field('終了', end)),
      utcPreview,
      h('div', { class: 'hint' }, '時刻は選んだタイムゾーンの壁時計で入れます。保存は UTC（RFC 3339）。窓は 24 時間まで。')),
    h('fieldset', null, h('legend', null, '準備と猶予'),
      h('div', { class: 'cols' }, field('準備（pre-roll、秒）', pre), field('後ろの余白（post-roll、秒）', post), field('遅れて始める猶予（秒）', grace)),
      snapField,
      field('録画の保持日数', retention)),
    h('fieldset', null, h('legend', null, '繰り返し'), rep, wdBox, repEndBox,
      h('div', { class: 'hint' }, '繰り返しは選んだタイムゾーンの壁時計で解釈します（夏時間でも同じ時刻）。')),
    errors);

  const intOr = (input, mult = 1) => (input.value === '' ? null : Math.round(Number(input.value) * mult));

  return {
    el,
    value() {
      const problems = [];
      if (!stream.value) problems.push('配信を選んでください（先に配信を作ってください）');
      const s = wallToUtcIso(start.value, tz.value);
      const e = wallToUtcIso(end.value, tz.value);
      if (!s || !e) problems.push('開始と終了を入れてください');
      else {
        if (new Date(e) <= new Date(s)) problems.push('終了は開始より後にしてください');
        if (new Date(e) - new Date(s) > 24 * 3600_000) problems.push('窓は 24 時間までです');
        if (new Date(e) <= new Date()) problems.push('終了が過去です');
      }
      const weekdays = wd.filter((x) => x.input.checked).map((x) => Number(x.input.value));
      if (rep.value === 'weekly' && !weekdays.length) problems.push('毎週の曜日を選んでください');
      replace(errors, problems.length ? h('div', { class: 'alert error' }, h('ul', null, problems.map((p) => h('li', null, p)))) : null);
      if (problems.length) return null;
      const body = {
        stream: stream.value,
        name: name.value.trim() || null,
        action: action.value,
        start_at: s,
        end_at: e,
        pre_roll_ms: intOr(pre, 1000),
        post_roll_ms: intOr(post, 1000),
        grace_ms: intOr(grace, 1000),
        snapshot_interval_secs: action.value === 'snapshot' ? intOr(snapInt) : null,
        retention_days: intOr(retention),
        recurrence: null,
      };
      if (rep.value !== 'none') {
        body.recurrence = {
          freq: rep.value,
          timezone: tz.value,
          weekdays: rep.value === 'weekly' ? weekdays : undefined,
          count: repEndKind.value === 'count' ? Number(count.value) : null,
          until_ms: repEndKind.value === 'until' && until.value
            ? new Date(wallToUtcIso(`${until.value}T00:00`, tz.value)).getTime() + 86400_000 : null,
        };
      }
      return body;
    },
  };
}

async function createDialog(streamList, preset) {
  const form = scheduleForm(streamList, preset);
  const result = h('div', { 'aria-live': 'polite' });
  const key = idemKey();
  const runCheck = async (body) => {
    try {
      const { data } = await schedules.check(body);
      replace(result, data.overlaps.length
        ? h('div', { class: 'alert warn' },
          h('strong', null, `重なる録画予約が ${data.overlaps.length} 件あります`), data.shared_input ? '（入力は共有します。接続は増えません）' : '',
          table([
            { label: '予約', render: (o) => h('code', null, o.id.slice(0, 10)) },
            { label: '開始', render: (o) => fmtTime(o.start_at) },
            { label: '終了', render: (o) => fmtTime(o.end_at) },
          ], data.overlaps))
        : h('div', { class: 'alert ok' }, '検査に通りました。重なる予約はありません。'));
      return true;
    } catch (e) {
      const overlaps = e.details && Array.isArray(e.details.overlaps) ? e.details.overlaps : [];
      replace(result, errorBox(e, { title: '検査に通りません' }),
        overlaps.length ? table([
          { label: '重なる予約', render: (o) => h('code', null, String(o.id || o).slice(0, 10)) },
          { label: '開始', render: (o) => (o.start_at ? fmtTime(o.start_at) : '') },
          { label: '終了', render: (o) => (o.end_at ? fmtTime(o.end_at) : '') },
        ], overlaps) : null);
      return false;
    }
  };
  return modal('予約を作成', () => [form.el, result], {
    wide: true,
    actions: (close) => {
      const check = h('button', { onClick: async () => { const b = form.value(); if (b) await runCheck(b); } }, '検査（重なりを確かめる）');
      const ok = h('button', { class: 'primary', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
        try {
          if (!(await runCheck(body))) return;
          const { data } = await schedules.create(body, key);
          close(data);
        } catch (e) {
          replace(result, errorBox(e, { title: '作成できませんでした' }));
        } finally {
          ok.disabled = false;
        }
      } }, '検査して作成');
      return [h('button', { onClick: () => close(null) }, 'やめる'), check, ok];
    },
  });
}

export async function render(root, _p, { navigate }) {
  const box = h('div');
  const pager = h('div', { class: 'pager' });
  let streamList = [];
  const names = new Map();
  try {
    streamList = await listAll(streams.list);
    for (const s of streamList) names.set(s.id, s.name);
  } catch (e) {
    reportError(e, '配信の一覧');
  }
  const streamSel = select([['', 'すべての配信'], ...streamList.map((s) => [s.id, s.name])], '');
  let cursors = [undefined];
  let page = 0;

  const refresh = () => load(box, async () => {
    const { data } = await schedules.list({ limit: PAGE, cursor: cursors[page], stream: streamSel.value || undefined });
    cursors[page + 1] = data.next_cursor || null;
    replace(pager,
      h('button', { class: 'small', disabled: page === 0, onClick: () => { page -= 1; refresh(); } }, '← 前へ'),
      h('span', { class: 'muted small' }, `${page + 1} ページ目`),
      h('button', { class: 'small', disabled: !data.next_cursor, onClick: () => { page += 1; refresh(); } }, '次へ →'));
    const items = data.items.slice().sort((a, b) => b.start_at.localeCompare(a.start_at));
    return table([
      { label: '予約', render: (s) => h('a', { href: `#/schedules/${encodeURIComponent(s.id)}` }, s.name || s.id.slice(0, 10)) },
      { label: '配信', render: (s) => names.get(s.stream) || h('code', null, s.stream.slice(0, 10)) },
      { label: '操作', render: (s) => ACTIONS[s.action] || s.action },
      { label: '開始', render: (s) => h('span', { title: s.recurrence ? fmtTimeIn(s.start_at, s.recurrence.timezone) + ` (${s.recurrence.timezone})` : s.start_at }, fmtTime(s.start_at), ' ', h('span', { class: 'muted small' }, fmtRelative(s.start_at))) },
      { label: '終了', render: (s) => fmtTime(s.end_at) },
      { label: '繰り返し', render: (s) => describeRecurrence(s.recurrence) },
      { label: '状態', render: (s) => schedBadge(s.state) },
      { label: '直近の実行', render: (s) => (s.last_run ? [schedBadge(s.last_run.state), ' ', h('span', { class: 'small muted' }, fmtTime(s.last_run.start_at))] : '—') },
    ], items, { empty: '予約はありません', caption: '予約の一覧' });
  });
  streamSel.addEventListener('change', () => { cursors = [undefined]; page = 0; refresh(); });

  const create = async () => {
    if (!streamList.length) {
      reportError(new Error('先に配信を作ってください'));
      return;
    }
    const s = await createDialog(streamList, streamSel.value || null);
    if (s) navigate(`/schedules/${encodeURIComponent(s.id)}`);
  };

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, '予約'), h('span', { class: 'spacer' }),
      h('button', { onClick: () => refresh() }, '再読み込み'),
      h('button', { class: 'primary', onClick: () => create().catch((e) => reportError(e, '作成')) }, '＋ 予約を作成')),
    h('section', { class: 'panel' }, h('div', { class: 'toolbar form' }, field('配信', streamSel)), box, pager));
  await refresh();
  return refreshOn(['schedule.'], refresh);
}
