// 予約の一覧と作成（作成前に POST /schedules/check で重なりを確かめる）。#/schedules?new=1 で作成のダイアログを開く。

import { h, replace, table, select, checkbox } from '../lib/dom.js';
import { load, modal, reportError, errorBox } from '../lib/ui.js';
import { schedules, streams, listAll } from '../api/resources.js';
import { fmtTime, fmtTimeIn, fmtRelative, wallToUtcIso, utcToWall, zoneChoices, localZone, weekdayNames, idemKey } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t } from '../i18n/index.js';
import {
  button, iconButton, pageHeader, dataTable, pagination, emptyState, badge, alert, formField, helpHint,
} from '../ui/index.js';
import { invalidatePalette } from '../palette.js';
import { onPageKey } from '../shortcuts.js';

const PAGE = 50;

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

function scheduleForm(streamList, preset) {
  const zone = localZone();
  const now = Date.now();
  const startDefault = Math.ceil((now + 5 * 60_000) / 300_000) * 300_000;
  const stream = select(streamList.map((s) => [s.id, s.name]), preset || (streamList[0] && streamList[0].id));
  const name = h('input', { type: 'text', maxlength: 128, placeholder: t('schedules.form.namePlaceholder') });
  const action = select(['record', 'live', 'snapshot'].map((a) => [a, actionLabel(a)]), 'record');
  const tz = select(zoneChoices().map((z) => [z, z === zone ? t('schedules.form.thisDevice', { zone: z }) : z]), zone);
  const start = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault, zone) });
  const end = h('input', { type: 'datetime-local', required: true, value: utcToWall(startDefault + 3600_000, zone) });
  const pre = h('input', { type: 'number', min: 0, max: 300, value: 30 });
  const post = h('input', { type: 'number', min: 0, max: 300, value: 0 });
  const grace = h('input', { type: 'number', min: 0, max: 3600, value: 60 });
  const snapInt = h('input', { type: 'number', min: 1, max: 3600, value: 60 });
  const retention = h('input', { type: 'number', min: 0, placeholder: t('schedules.form.retentionDefault') });
  const rep = select([['none', t('schedules.form.repNone')], ['daily', t('schedules.rec.daily')], ['weekly', t('schedules.rec.weekly')]], 'none', { 'aria-label': t('schedules.form.repeat') });
  const wd = weekdayNames().map((d, i) => checkbox(d, false, { value: String(i) }));
  const repEndKind = select([['none', t('schedules.form.endNone')], ['count', t('schedules.form.endCount')], ['until', t('schedules.form.endUntil')]], 'none', { 'aria-label': t('schedules.form.repeatEnd') });
  const count = h('input', { type: 'number', min: 1, value: 10, 'aria-label': t('schedules.form.endCount') });
  const until = h('input', { type: 'date', 'aria-label': t('schedules.form.endUntil') });
  const utcPreview = h('div', { class: 'hint', 'aria-live': 'polite' });
  rep.style.justifySelf = 'start';

  const wdBox = h('div', { class: 'inline', role: 'group', 'aria-label': t('schedules.form.weekdays') }, wd.map((x) => x.el));
  const repEndBox = h('div', { class: 'inline' }, repEndKind, count, until);
  const snapField = formField({ label: t('schedules.form.snapInterval'), input: snapInt });

  const sync = () => {
    wdBox.hidden = rep.value !== 'weekly';
    repEndBox.hidden = rep.value === 'none';
    count.hidden = repEndKind.value !== 'count';
    until.hidden = repEndKind.value !== 'until';
    snapField.hidden = action.value !== 'snapshot';
    const s = wallToUtcIso(start.value, tz.value);
    const e = wallToUtcIso(end.value, tz.value);
    utcPreview.textContent = s && e ? `UTC: ${s} 〜 ${e}${tz.value !== zone ? t('schedules.form.localPreview', { start: fmtTime(s), end: fmtTime(e) }) : ''}` : '';
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

  const errors = h('div');
  const el = h('div', { class: 'form' },
    h('div', { class: 'cols' },
      formField({ label: t('schedules.col.stream'), input: stream, required: true }),
      formField({ label: t('schedules.col.action'), input: action, help: helpHint({ title: t('schedules.col.action'), body: t('schedules.form.actionHelp'), guide: '05-recording.md' }) }),
      formField({ label: t('schedules.form.nameOptional'), input: name })),
    h('fieldset', null, h('legend', null, t('schedules.form.time')),
      h('div', { class: 'cols' },
        formField({ label: t('schedules.form.timezone'), input: tz }),
        formField({ label: t('schedules.col.start'), input: start, required: true }),
        formField({ label: t('schedules.col.end'), input: end, required: true })),
      utcPreview,
      h('div', { class: 'hint' }, t('schedules.form.timeHint'))),
    h('fieldset', null, h('legend', null, t('schedules.form.margins')),
      h('div', { class: 'cols' },
        formField({ label: t('schedules.form.pre'), input: pre }),
        formField({ label: t('schedules.form.post'), input: post }),
        formField({ label: t('schedules.form.grace'), input: grace })),
      snapField,
      formField({ label: t('schedules.form.retention'), input: retention })),
    h('fieldset', null, h('legend', null, t('schedules.form.repeat')), rep, wdBox, repEndBox,
      h('div', { class: 'hint' }, t('schedules.form.repeatHint'))),
    errors);

  const intOr = (input, mult = 1) => (input.value === '' ? null : Math.round(Number(input.value) * mult));

  return {
    el,
    value() {
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
      replace(errors, problems.length ? alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { title: t('streams.form.fixBelow') }) : null);
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
  const overlapTable = (rows) => table([
    { label: t('schedules.check.schedule'), render: (o) => h('code', null, String(o.id || o).slice(0, 10)) },
    { label: t('schedules.col.start'), render: (o) => (o.start_at ? fmtTime(o.start_at) : '') },
    { label: t('schedules.col.end'), render: (o) => (o.end_at ? fmtTime(o.end_at) : '') },
  ], rows);
  const runCheck = async (body) => {
    try {
      const { data } = await schedules.check(body);
      replace(result, data.overlaps.length
        ? alert('warn', [data.shared_input ? t('schedules.check.shared') : '', overlapTable(data.overlaps)], { title: t('schedules.check.overlaps', { n: data.overlaps.length }) })
        : alert('ok', t('schedules.check.ok')));
      return true;
    } catch (e) {
      const overlaps = e.details && Array.isArray(e.details.overlaps) ? e.details.overlaps : [];
      replace(result, errorBox(e, { title: t('schedules.check.failed') }), overlaps.length ? overlapTable(overlaps) : null);
      return false;
    }
  };
  return modal(t('schedules.createTitle'), () => [form.el, result], {
    wide: true,
    icon: 'schedules',
    testid: 'schedule-create-dialog',
    actions: (close) => {
      const check = h('button', { type: 'button', onClick: async () => { const b = form.value(); if (b) await runCheck(b); } }, t('schedules.check.run'));
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'schedule-create-submit', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
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
      } }, t('schedules.checkAndCreate'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), check, ok];
    },
  });
}

export async function render(root, _p, { navigate }) {
  const box = h('div');
  const pager = h('div');
  let streamList = [];
  const names = new Map();
  try {
    streamList = await listAll(streams.list);
    for (const s of streamList) names.set(s.id, s.name);
  } catch (e) {
    reportError(e, t('schedules.streamList'));
  }
  const streamSel = select([['', t('schedules.allStreams')], ...streamList.map((s) => [s.id, s.name])], '');
  let cursors = [undefined];
  let page = 0;

  const create = async () => {
    if (!streamList.length) {
      reportError(new Error(t('schedules.needStream')));
      return;
    }
    const s = await createDialog(streamList, streamSel.value || null);
    if (s) navigate(`/schedules/${encodeURIComponent(s.id)}`);
  };
  const createBtn = (testid) => button(t('schedules.create'), { variant: 'primary', icon: 'plus', testid, onClick: () => create().catch((e) => reportError(e, t('common.create'))) });

  const columns = [
    { key: 'name', label: t('schedules.col.schedule'), primary: true, sort: (s) => s.name || s.id, render: (s) => h('a', { href: `#/schedules/${encodeURIComponent(s.id)}` }, s.name || s.id.slice(0, 10)) },
    { key: 'stream', label: t('schedules.col.stream'), sort: (s) => names.get(s.stream) || s.stream, render: (s) => names.get(s.stream) || h('code', null, s.stream.slice(0, 10)) },
    { key: 'action', label: t('schedules.col.action'), sort: (s) => s.action, render: (s) => actionLabel(s.action) },
    { key: 'start', label: t('schedules.col.start'), sort: (s) => s.start_at, render: (s) => h('span', { title: s.recurrence ? `${fmtTimeIn(s.start_at, s.recurrence.timezone)} (${s.recurrence.timezone})` : s.start_at }, fmtTime(s.start_at), ' ', h('span', { class: 'muted small' }, fmtRelative(s.start_at))) },
    { key: 'end', label: t('schedules.col.end'), sort: (s) => s.end_at, render: (s) => fmtTime(s.end_at) },
    { key: 'rec', label: t('schedules.col.repeat'), wrap: true, render: (s) => describeRecurrence(s.recurrence) },
    { key: 'state', label: t('common.state'), sort: (s) => s.state, render: (s) => schedBadge(s.state) },
    { key: 'last', label: t('schedules.col.lastRun'), render: (s) => (s.last_run ? h('span', { class: 'row' }, schedBadge(s.last_run.state), h('span', { class: 'small muted' }, fmtTime(s.last_run.start_at))) : '—') },
  ];

  const refresh = () => load(box, async () => {
    const { data } = await schedules.list({ limit: PAGE, cursor: cursors[page], stream: streamSel.value || undefined });
    cursors[page + 1] = data.next_cursor || null;
    replace(pager, pagination({
      page, hasPrev: page > 0, hasNext: !!data.next_cursor,
      onPrev: () => { page -= 1; refresh(); }, onNext: () => { page += 1; refresh(); },
    }));
    const items = data.items.slice().sort((a, b) => b.start_at.localeCompare(a.start_at));
    if (!items.length && page === 0 && !streamSel.value) {
      return emptyState({
        icon: 'schedules', title: t('schedules.emptyTitle'), body: streamList.length ? t('schedules.emptyBody') : t('schedules.emptyNoStream'), testid: 'schedules-empty',
        actions: [streamList.length ? createBtn() : h('a', { class: 'btn primary', href: '#/streams?new=1' }, t('streams.create'))],
      });
    }
    return dataTable({ columns, rows: items, caption: t('schedules.caption'), rowKey: (s) => s.id, testid: 'schedules-table', empty: { icon: 'schedules', title: t('schedules.emptyFiltered') } });
  }, { kind: 'table' });
  streamSel.addEventListener('change', () => { cursors = [undefined]; page = 0; refresh(); });

  root.append(
    pageHeader({
      title: t('nav.schedules'),
      description: t('schedules.lead'),
      help: helpHint({ title: t('nav.schedules'), body: t('schedules.help'), guide: '05-recording.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() }),
        createBtn('schedules-create'),
      ],
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'toolbar form' }, formField({ label: t('schedules.col.stream'), input: streamSel })),
      box, pager));
  await refresh();
  if (new URLSearchParams(location.hash.split('?')[1] || '').get('new') === '1') {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/schedules`);
    create().catch((e) => reportError(e, t('common.create')));
  }
  const offKey = onPageKey('n', () => create().catch((e) => reportError(e, t('common.create'))));
  const off = refreshOn(['schedule.'], refresh);
  return () => { off(); offKey(); };
}
