// 予約: これからの予定（7 日の時間割と次の予定の一覧）と、すべての予約の表。作成のダイアログは入力するたびに重なりを確かめる。
//
// - 繰り返しの予約は、ノードと同じ決まり（選んだタイムゾーンの壁時計・曜日・回数・終わりの日）でこの画面が展開する。
//   API は予約 1 件を返すだけで、これからの実行の一覧は無い。
// - 時刻は「この端末の時刻」で並べ、予約のタイムゾーンが違えばそちらの時刻も添える。
// - 表示の切り替え（?view=upcoming|all）と配信の絞り込み（?stream=）は URL に残す。

import { h, replace, table, select, checkbox, debounce } from '../lib/dom.js';
import { load, modal, reportError, errorBox } from '../lib/ui.js';
import { schedules, streams, listAll } from '../api/resources.js';
import {
  fmtTime, fmtTimeIn, fmtRelative, fmtHm, fmtDay, fmtDurationText, wallToUtcIso, utcToWall, zoneChoices, localZone, weekdayNames, idemKey,
} from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t } from '../i18n/index.js';
import {
  button, pageHeader, dataTable, emptyState, badge, alert, formField, helpHint, segmented, listNav, tooltip,
} from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { invalidatePalette } from '../palette.js';
import { onPageKey } from '../shortcuts.js';
import { hashParams, setHashParams } from '../lib/streaming.js';

const DAY = 86400_000;
export const ACTION_ICON = { record: 'record', live: 'live', snapshot: 'image' };

/** 操作の名前（record・live・snapshot）。 */
export function actionLabel(a) {
  return ['record', 'live', 'snapshot'].includes(a) ? t(`schedules.action.${a}`) : a;
}

// 互換: 以前の ACTIONS（値 → 名前）。言語に合わせて引く
export const ACTIONS = new Proxy({}, { get: (_, k) => (typeof k === 'string' && ['record', 'live', 'snapshot'].includes(k) ? t(`schedules.action.${k}`) : undefined) });

const STATE_KIND = { running: 'ok', pending: 'info', completed: 'ok', missed: 'warn', canceled: 'outline', failed: 'danger' };

/** 予約・実行の状態のピル。 */
export function schedBadge(state) {
  const known = state in STATE_KIND;
  return badge(known ? t(`schedules.state.${state}`) : String(state), STATE_KIND[state], { dot: true });
}

export function describeRecurrence(r) {
  if (!r) return t('schedules.rec.once');
  const names = weekdayNames();
  const days = r.freq === 'weekly' && r.weekdays && r.weekdays.length ? t('schedules.rec.days', { days: r.weekdays.map((d) => names[d]).join(t('schedules.rec.daySep')) }) : '';
  const end = r.count ? t('schedules.rec.count', { n: r.count }) : r.until_ms ? t('schedules.rec.until', { date: fmtTime(new Date(r.until_ms).toISOString()) }) : '';
  return t('schedules.rec.summary', { freq: r.freq === 'daily' ? t('schedules.rec.daily') : t('schedules.rec.weekly'), days, tz: r.timezone, end });
}

/**
 * 予約の実行の時刻（UTC ミリ秒の {start, end, n}）を [from, to) の範囲で返す。繰り返しは選んだタイムゾーンの壁時計で数える。
 * n は 1 から数えた回。予約の start_at が 1 回目。
 */
export function occurrences(s, from, to, max = 60) {
  const first = new Date(s.start_at).getTime();
  const dur = new Date(s.end_at).getTime() - first;
  const out = [];
  if (!s.recurrence) {
    if (first + dur > from && first < to) out.push({ start: first, end: first + dur, n: 1 });
    return out;
  }
  const r = s.recurrence;
  const tz = r.timezone || 'UTC';
  const [d0, time] = utcToWall(first, tz).split('T');
  const [y, m, d] = d0.split('-').map(Number);
  const base = Date.UTC(y, m - 1, d);
  let n = 0;
  for (let i = 0; i < 1500 && out.length < max; i++) {
    const day = new Date(base + i * DAY);
    const wd = day.getUTCDay();
    if (r.freq === 'weekly') {
      if (r.weekdays && r.weekdays.length) { if (!r.weekdays.includes(wd)) continue; } else if (i % 7 !== 0) continue;
    }
    // 壁時計は分までなので、秒以下は 1 回目と同じにする
    const st = new Date(wallToUtcIso(`${day.toISOString().slice(0, 10)}T${time}`, tz)).getTime() + (((first % 60_000) + 60_000) % 60_000);
    if (st < first) continue;
    n += 1;
    if (r.count && n > r.count) break;
    if (r.until_ms && st >= r.until_ms) break;
    if (st >= to) break;
    if (st + dur > from) out.push({ start: st, end: st + dur, n });
  }
  return out;
}

/** 予約の時刻の説明（この端末の時刻 ＋ 予約のタイムゾーンが違えばその時刻）。 */
export function whenText(startMs, endMs, tz) {
  const zone = localZone();
  const local = `${fmtDay(startMs)} ${fmtHm(startMs)}–${fmtHm(endMs)}`;
  if (!tz || tz === zone) return local;
  return t('schedules.when.withZone', { local, other: `${fmtHm(startMs, tz)}–${fmtHm(endMs, tz)}`, tz });
}

function scheduleForm(streamList, preset, onChange) {
  const zone = localZone();
  const now = Date.now();
  const startDefault = Math.ceil((now + 5 * 60_000) / 300_000) * 300_000;
  const stream = select(streamList.map((s) => [s.id, s.name]), preset || (streamList[0] && streamList[0].id), { 'data-testid': 'sched-stream' });
  const name = h('input', { type: 'text', maxlength: 128, placeholder: t('schedules.form.namePlaceholder') });
  const action = segmented({
    label: t('schedules.col.action'), testid: 'sched-action', value: 'record',
    options: ['record', 'live', 'snapshot'].map((a) => [a, actionLabel(a), ACTION_ICON[a]]),
    onChange: () => sync(),
  });
  const tz = select(zoneChoices().map((z) => [z, z === zone ? t('schedules.form.thisDevice', { zone: z }) : z]), zone);
  const start = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault, zone) });
  const end = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault + 3600_000, zone) });
  const pre = h('input', { type: 'number', min: 0, max: 300, value: 30 });
  const post = h('input', { type: 'number', min: 0, max: 300, value: 0 });
  const grace = h('input', { type: 'number', min: 0, max: 3600, value: 60 });
  const snapInt = h('input', { type: 'number', min: 1, max: 3600, value: 60 });
  const retention = h('input', { type: 'number', min: 0, placeholder: t('schedules.form.retentionDefault') });
  const rep = segmented({
    label: t('schedules.form.repeat'), testid: 'sched-repeat', value: 'none',
    options: [['none', t('schedules.form.repNone')], ['daily', t('schedules.rec.daily')], ['weekly', t('schedules.rec.weekly')]],
    onChange: () => sync(),
  });
  const wd = weekdayNames().map((d, i) => checkbox(d, false, { value: String(i) }));
  const repEndKind = select([['none', t('schedules.form.endNone')], ['count', t('schedules.form.endCount')], ['until', t('schedules.form.endUntil')]], 'none', { 'aria-label': t('schedules.form.repeatEnd') });
  const count = h('input', { type: 'number', min: 1, value: 10, 'aria-label': t('schedules.form.endCount') });
  const until = h('input', { type: 'date', 'aria-label': t('schedules.form.endUntil') });
  const preview = h('div', { class: 'when-preview', 'aria-live': 'polite', 'data-testid': 'sched-when' });

  const wdBox = h('div', { class: 'weekday-picks', role: 'group', 'aria-label': t('schedules.form.weekdays') }, wd.map((x) => x.el));
  const repEndBox = h('div', { class: 'inline' }, repEndKind, count, until);
  const snapField = formField({ label: t('schedules.form.snapInterval'), input: snapInt });
  const retField = formField({ label: t('schedules.form.retention'), input: retention });

  const sync = () => {
    wdBox.hidden = rep.value !== 'weekly';
    repEndBox.hidden = rep.value === 'none';
    count.hidden = repEndKind.value !== 'count';
    until.hidden = repEndKind.value !== 'until';
    snapField.hidden = action.value !== 'snapshot';
    retField.hidden = action.value !== 'record';
    const s = wallToUtcIso(start.value, tz.value);
    const e = wallToUtcIso(end.value, tz.value);
    if (s && e && new Date(e) > new Date(s)) {
      const sm = new Date(s).getTime();
      const em = new Date(e).getTime();
      replace(preview,
        icon('clock', { size: 'sm' }),
        h('div', null,
          h('strong', null, `${fmtDay(sm, tz.value)} ${fmtHm(sm, tz.value)}–${fmtHm(em, tz.value)}`, ' ', h('span', { class: 'muted' }, `(${tz.value})`), ` · ${fmtDurationText(em - sm)}`),
          tz.value !== zone ? h('div', { class: 'small muted' }, t('schedules.form.localIs', { when: `${fmtDay(sm)} ${fmtHm(sm)}–${fmtHm(em)}`, zone })) : null,
          h('div', { class: 'small muted' }, sm > Date.now() ? t('schedules.form.startsIn', { rel: fmtRelative(s) }) : t('schedules.form.startsNow'))));
    } else replace(preview);
    if (onChange) onChange();
  };
  for (const el of [repEndKind, start, end, tz, stream, count, until, pre, post]) { el.addEventListener('input', sync); el.addEventListener('change', sync); }
  for (const x of wd) x.input.addEventListener('change', sync);
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

  const errors = h('div');
  const el = h('div', { class: 'form' },
    h('div', { class: 'cols' },
      formField({ label: t('schedules.col.stream'), input: stream, required: true }),
      formField({ label: t('schedules.form.nameOptional'), input: name })),
    h('div', { class: 'field' }, h('div', { class: 'field-label-row' }, h('span', { class: 'label' }, t('schedules.col.action')), helpHint({ title: t('schedules.col.action'), body: t('schedules.form.actionHelp'), guide: '05-recording.md' })), action),
    h('fieldset', null, h('legend', null, t('schedules.form.time')),
      h('div', { class: 'cols' },
        formField({ label: t('schedules.col.start'), input: start, required: true }),
        formField({ label: t('schedules.col.end'), input: end, required: true }),
        formField({ label: t('schedules.form.timezone'), input: tz, help: helpHint({ title: t('schedules.form.timezone'), body: t('schedules.form.timeHint') }) })),
      preview),
    h('fieldset', null, h('legend', null, t('schedules.form.repeat')), rep, wdBox, repEndBox,
      h('div', { class: 'hint' }, t('schedules.form.repeatHint'))),
    h('details', { class: 'disclosure' },
      h('summary', null, icon('settings'), h('span', null, t('schedules.form.margins')), h('small', null, t('schedules.form.marginsHint'))),
      h('div', { class: 'form' },
        h('div', { class: 'cols' },
          formField({ label: t('schedules.form.pre'), input: pre }),
          formField({ label: t('schedules.form.post'), input: post }),
          formField({ label: t('schedules.form.grace'), input: grace })),
        snapField, retField)),
    errors);

  const intOr = (input, mult = 1) => (input.value === '' ? null : Math.round(Number(input.value) * mult));

  /** 本文を作る。silent なら問題を表示しない（入力中の自動の検査）。 */
  const build = (silent) => {
    const problems = [];
    if (!stream.value) problems.push(t('schedules.form.errStream'));
    const s = wallToUtcIso(start.value, tz.value);
    const e = wallToUtcIso(end.value, tz.value);
    if (!s || !e) problems.push(t('schedules.form.errTimes'));
    else {
      if (new Date(e) <= new Date(s)) problems.push(t('schedules.form.errOrder'));
      if (new Date(e) - new Date(s) > 24 * 3600_000) problems.push(t('schedules.form.errWindow'));
      if (new Date(e) <= new Date()) problems.push(t('schedules.form.errPast'));
    }
    const weekdays = wd.filter((x) => x.input.checked).map((x) => Number(x.input.value));
    if (rep.value === 'weekly' && !weekdays.length) problems.push(t('schedules.form.errWeekdays'));
    if (!silent) replace(errors, problems.length ? alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { title: t('streams.form.fixBelow') }) : null);
    if (problems.length) return silent ? { problems } : null;
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
          ? new Date(wallToUtcIso(`${until.value}T00:00`, tz.value)).getTime() + DAY : null,
      };
    }
    return silent ? { body } : body;
  };
  sync();

  return { el, value: () => build(false), peek: () => build(true), clearErrors: () => replace(errors) };
}

async function createDialog(streamList, preset) {
  const result = h('div', { class: 'check-result', 'aria-live': 'polite', 'data-testid': 'sched-check' });
  const key = idemKey();
  let seq = 0;
  let lastOk = null;
  const overlapTable = (rows) => table([
    { label: t('schedules.check.schedule'), render: (o) => h('a', { href: `#/schedules/${encodeURIComponent(o.id || o)}`, target: '_blank' }, String(o.id || o).slice(0, 10)) },
    { label: t('schedules.col.start'), render: (o) => (o.start_at ? fmtTime(o.start_at) : '') },
    { label: t('schedules.col.end'), render: (o) => (o.end_at ? fmtTime(o.end_at) : '') },
  ], rows);
  const runCheck = async (body) => {
    const my = ++seq;
    replace(result, h('div', { class: 'check-line', 'data-state': 'checking' }, icon('spinner', { spin: true }), t('schedules.check.checking')));
    try {
      const { data } = await schedules.check(body);
      if (my !== seq) return lastOk;
      replace(result, data.overlaps.length
        ? alert('warn', [data.shared_input ? t('schedules.check.shared') : t('schedules.check.overlapNote'), overlapTable(data.overlaps)], { title: t('schedules.check.overlaps', { n: data.overlaps.length }), small: true })
        : h('div', { class: 'check-line ok', 'data-state': 'ok' }, icon('success'), t('schedules.check.ok')));
      lastOk = true;
      return true;
    } catch (e) {
      if (my !== seq) return lastOk;
      const overlaps = e.details && Array.isArray(e.details.overlaps) ? e.details.overlaps : [];
      replace(result, errorBox(e, { title: t('schedules.check.failed') }), overlaps.length ? overlapTable(overlaps) : null);
      lastOk = false;
      return false;
    }
  };
  // 入力が整ったら自動で検査する（打つたびには送らない）
  const auto = debounce(() => {
    const r = form.peek();
    if (r.body) runCheck(r.body);
    else replace(result, h('div', { class: 'check-line', 'data-state': 'incomplete' }, icon('info'), t('schedules.check.incomplete')));
  }, 500);
  const form = scheduleForm(streamList, preset, () => auto());
  return modal(t('schedules.createTitle'), () => [form.el, result], {
    wide: true,
    icon: 'schedules',
    testid: 'schedule-create-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'schedule-create-submit', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
        auto.cancel();
        try {
          if (!(await runCheck(body))) return;
          const { data } = await schedules.create(body, key);
          invalidatePalette();
          close(data);
        } catch (e) {
          replace(result, errorBox(e, { title: t('schedules.createFailed') }));
        } finally {
          ok.disabled = false;
        }
      } }, icon('plus'), t('schedules.create'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  }).finally(() => auto.cancel());
}

// ---------------------------------------------------------------- これからの予定（7 日の時間割）
function upcomingView(list, names, { days = 7 } = {}) {
  const zone = localZone();
  const now = Date.now();
  const today = new Date(wallToUtcIso(`${utcToWall(now, zone).slice(0, 10)}T00:00`, zone)).getTime();
  const end = today + days * DAY + DAY;
  const active = list.filter((s) => s.state === 'pending' || s.state === 'running');
  const occ = [];
  for (const s of active) {
    for (const o of occurrences(s, now - DAY, end, 40)) occ.push({ ...o, s });
  }
  occ.sort((a, b) => a.start - b.start);
  if (!occ.length) return null;

  const dayStarts = Array.from({ length: days }, (_, i) => new Date(wallToUtcIso(`${utcToWall(today + i * DAY + 3600_000 * 12, zone).slice(0, 10)}T00:00`, zone)).getTime());
  const block = (o, d0, d1) => {
    const a = Math.max(o.start, d0);
    const b = Math.min(o.end, d1);
    const el = h('a', {
      class: `tt-block act-${o.s.action}${o.s.state === 'running' && o.start <= now && o.end > now ? ' running' : ''}`,
      href: `#/schedules/${encodeURIComponent(o.s.id)}`,
      'data-testid': 'tt-block',
      'aria-label': `${o.s.name || actionLabel(o.s.action)} ${whenText(o.start, o.end, o.s.recurrence && o.s.recurrence.timezone)}`,
    }, icon(ACTION_ICON[o.s.action] || 'clock', { size: 'sm' }), h('span', null, o.s.name || names.get(o.s.stream) || actionLabel(o.s.action)));
    el.style.left = `${(((a - d0) / (d1 - d0)) * 100).toFixed(2)}%`;
    el.style.width = `max(6px, ${(((b - a) / (d1 - d0)) * 100).toFixed(2)}%)`;
    tooltip(el, `${o.s.name || actionLabel(o.s.action)} · ${names.get(o.s.stream) || ''} · ${fmtHm(o.start)}–${fmtHm(o.end)}`);
    return el;
  };
  const rows = dayStarts.map((d0, i) => {
    const d1 = i + 1 < dayStarts.length ? dayStarts[i + 1] : d0 + DAY;
    const track = h('div', { class: 'tt-track' },
      [6, 12, 18].map((hr) => { const x = h('span', { class: 'tt-tick', 'aria-hidden': 'true' }); x.style.left = `${(hr / 24) * 100}%`; return x; }));
    for (const o of occ) if (o.end > d0 && o.start < d1) track.append(block(o, d0, d1));
    if (now >= d0 && now < d1) {
      const n = h('span', { class: 'tt-now', 'aria-hidden': 'true' });
      n.style.left = `${(((now - d0) / (d1 - d0)) * 100).toFixed(2)}%`;
      track.append(n);
    }
    return h('div', { class: ['tt-row', i === 0 && 'today'].filter(Boolean).join(' ') },
      h('div', { class: 'tt-day' }, i === 0 ? t('schedules.tt.today') : i === 1 ? t('schedules.tt.tomorrow') : fmtDay(d0 + 3600_000 * 12), i <= 1 ? h('small', null, fmtDay(d0 + 3600_000 * 12)) : null),
      track);
  });
  const scale = h('div', { class: 'tt-row tt-scale', 'aria-hidden': 'true' }, h('div', { class: 'tt-day' }),
    h('div', { class: 'tt-hours' }, [0, 6, 12, 18, 24].map((hr) => { const x = h('span', null, `${hr}:00`); x.style.left = `${(hr / 24) * 100}%`; return x; })));

  // 次の予定（この端末の時刻で、近い順）
  const next = occ.filter((o) => o.end > now).slice(0, 8);
  const agenda = h('ol', { class: 'agenda', 'data-testid': 'sched-agenda' }, next.map((o) => {
    const tz = o.s.recurrence ? o.s.recurrence.timezone : null;
    const runningNow = o.start <= now && o.end > now;
    return h('li', { 'data-key': o.s.id },
      h('div', { class: `ag-icon act-${o.s.action}` }, icon(ACTION_ICON[o.s.action] || 'clock')),
      h('div', { class: 'ag-main' },
        h('a', { href: `#/schedules/${encodeURIComponent(o.s.id)}`, class: 'ag-title' }, o.s.name || t('schedules.detail.untitled', { id: o.s.id.slice(0, 8) })),
        h('div', { class: 'ag-sub' }, `${actionLabel(o.s.action)} · ${names.get(o.s.stream) || o.s.stream.slice(0, 10)}${o.s.recurrence ? ` · ${o.s.recurrence.freq === 'daily' ? t('schedules.rec.daily') : t('schedules.rec.weekly')}` : ''}`)),
      h('div', { class: 'ag-when' },
        h('strong', { class: 'num' }, whenText(o.start, o.end, tz)),
        h('span', { class: runningNow ? 'text-ok' : 'muted' }, runningNow ? t('schedules.tt.now') : fmtRelative(new Date(o.start).toISOString()))));
  }));
  return h('div', { class: 'upcoming' },
    h('div', { class: 'timetable', role: 'group', 'aria-label': t('schedules.tt.label', { zone }) }, scale, rows),
    h('p', { class: 'small muted tt-zone' }, icon('globe', { size: 'sm' }), ' ', t('schedules.tt.zoneNote', { zone })),
    h('h3', null, t('schedules.tt.next')),
    agenda);
}

export async function render(root, _p, { navigate }) {
  const params = hashParams();
  const box = h('div');
  let streamList = [];
  const names = new Map();
  let view = params.get('view') === 'all' ? 'all' : 'upcoming';
  let items = [];
  try {
    streamList = await listAll(streams.list);
    for (const s of streamList) names.set(s.id, s.name);
  } catch (e) {
    reportError(e, t('schedules.streamList'));
  }
  const streamSel = select([['', t('schedules.allStreams')], ...streamList.map((s) => [s.id, s.name])], params.get('stream') || '', { 'aria-label': t('schedules.col.stream'), 'data-testid': 'sched-filter-stream' });

  const create = async () => {
    if (!streamList.length) {
      reportError(new Error(t('schedules.needStream')));
      return;
    }
    const s = await createDialog(streamList, streamSel.value || null);
    if (s) navigate(`/schedules/${encodeURIComponent(s.id)}`);
  };
  const createBtn = (testid) => button(t('schedules.create'), { variant: 'primary', icon: 'plus', testid, kbd: testid ? 'N' : undefined, onClick: () => create().catch((e) => reportError(e, t('common.create'))) });

  const columns = [
    { key: 'name', label: t('schedules.col.schedule'), primary: true, sort: (s) => s.name || s.id, render: (s) => h('div', { class: 'stream-cell' },
      h('div', { class: `thumb sm act-${s.action}`, 'aria-hidden': 'true' }, icon(ACTION_ICON[s.action] || 'clock')),
      h('div', { class: 'sc-text' },
        h('a', { class: 'sc-name', href: `#/schedules/${encodeURIComponent(s.id)}` }, s.name || t('schedules.detail.untitled', { id: s.id.slice(0, 8) })),
        h('div', { class: 'sc-sub' }, `${actionLabel(s.action)} · ${names.get(s.stream) || s.stream.slice(0, 10)}`),
        h('div', { class: 'sc-mobile' }, schedBadge(s.state), h('span', { class: 'small' }, fmtDay(s.start_at), ' ', fmtHm(s.start_at))))) },
    { key: 'start', label: t('schedules.col.when'), class: 'hide-sm', sort: (s) => s.start_at, render: (s) => h('span', { title: s.recurrence ? `${fmtTimeIn(s.start_at, s.recurrence.timezone)} (${s.recurrence.timezone})` : fmtTime(s.start_at) },
      whenText(new Date(s.start_at).getTime(), new Date(s.end_at).getTime(), s.recurrence && s.recurrence.timezone), h('div', { class: 'muted small' }, fmtRelative(s.start_at))) },
    { key: 'rec', label: t('schedules.col.repeat'), class: 'hide-sm', wrap: true, render: (s) => describeRecurrence(s.recurrence) },
    { key: 'state', label: t('common.state'), class: 'hide-sm', sort: (s) => s.state, render: (s) => schedBadge(s.state) },
    { key: 'last', label: t('schedules.col.lastRun'), class: 'hide-sm', render: (s) => (s.last_run ? h('span', { class: 'row' }, schedBadge(s.last_run.state), h('span', { class: 'small muted' }, fmtTime(s.last_run.start_at))) : h('span', { class: 'subtle' }, '—')) },
  ];

  const draw = () => {
    const list = streamSel.value ? items.filter((s) => s.stream === streamSel.value) : items;
    if (!items.length) {
      replace(box, emptyState({
        icon: 'schedules', title: t('schedules.emptyTitle'), body: streamList.length ? t('schedules.emptyBody') : t('schedules.emptyNoStream'), testid: 'schedules-empty',
        actions: [streamList.length ? createBtn() : h('a', { class: 'btn primary', href: '#/streams?new=1' }, t('streams.create'))],
      }));
      return;
    }
    if (view === 'upcoming') {
      const up = upcomingView(list, names);
      replace(box, up || emptyState({
        compact: true, icon: 'calendar', title: t('schedules.tt.emptyTitle'), body: t('schedules.tt.emptyBody'), testid: 'schedules-upcoming-empty',
        actions: [button(t('schedules.tt.showAll'), { size: 'sm', onClick: () => { viewSeg.set('all'); view = 'all'; setHashParams({ view: 'all' }); draw(); } })],
      }));
      return;
    }
    replace(box, dataTable({
      columns, rows: list.slice().sort((a, b) => b.start_at.localeCompare(a.start_at)), caption: t('schedules.caption'), rowKey: (s) => s.id, testid: 'schedules-table',
      sort: { key: 'start', dir: 'desc' },
      empty: { icon: 'schedules', title: t('schedules.emptyFiltered') },
    }));
  };

  const refresh = () => load(box, async () => {
    items = await listAll(schedules.list);
    draw();
    return [...box.childNodes];
  }, { kind: 'table' });
  streamSel.addEventListener('change', () => { setHashParams({ stream: streamSel.value || null }); draw(); });
  const viewSeg = segmented({
    label: t('schedules.view.label'), testid: 'sched-view', value: view,
    options: [['upcoming', t('schedules.view.upcoming'), 'calendar'], ['all', t('schedules.view.all'), 'list']],
    onChange: (v) => { view = v; setHashParams({ view: v === 'all' ? 'all' : null }); draw(); },
  });

  root.append(
    pageHeader({
      title: t('nav.schedules'),
      description: t('schedules.lead'),
      help: helpHint({ title: t('nav.schedules'), body: t('schedules.help'), guide: '05-recording.md' }),
      actions: [createBtn('schedules-create')],
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'table-toolbar' }, viewSeg, streamSel),
      box));
  await refresh();
  if (hashParams().get('new') === '1') {
    setHashParams({ new: null });
    create().catch((e) => reportError(e, t('common.create')));
  }
  const offs = [
    onPageKey('n', () => create().catch((e) => reportError(e, t('common.create')))),
    refreshOn(['schedule.'], refresh),
    listNav(box, { rows: () => [...box.querySelectorAll('tbody tr, .agenda li')], open: (r) => navigate(`/schedules/${encodeURIComponent(r.dataset.key)}`), key: (r) => `${r.dataset.key}:${[...r.parentNode.children].indexOf(r)}` }),
  ];
  // 「何分後」の表示と時間割の「今」の線を進める
  const timer = setInterval(() => { if (document.visibilityState === 'visible' && view === 'upcoming' && !box.querySelector(':focus')) draw(); }, 60_000);
  return () => { offs.forEach((off) => off()); clearInterval(timer); };
}
