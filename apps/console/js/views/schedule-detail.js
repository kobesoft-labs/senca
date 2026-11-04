// 予約の詳細: 内容・実行履歴・終了時刻の変更・取り消し・削除。

import { h, replace, select } from '../lib/dom.js';
import { load, confirmDialog, reportError, errorBox, toast } from '../lib/ui.js';
import { schedules, streams } from '../api/resources.js';
import { fmtTime, fmtTimeIn, fmtRelative, fmtDurationText, wallToUtcIso, utcToWall, zoneChoices, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { actionLabel, schedBadge, describeRecurrence, occurrences, whenText, ACTION_ICON } from './schedules.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, kvList, dataTable, formField } from '../ui/index.js';
import { rememberRecent, forgetRecent } from '../lib/recent.js';
import { invalidatePalette } from '../palette.js';

export async function render(root, [id], { navigate }) {
  let sch = null;
  let etag = null;
  let streamName = null;
  const head = h('div');
  const hero = h('div');
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
    toast(t('schedules.detail.stale'), 'warn', 8000);
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
      if (!(await on412(e))) reportError(e, t('common.updated'));
    }
  };

  const cancel = async () => {
    if (!(await confirmDialog(t('schedules.detail.cancelTitle'), t('schedules.detail.cancelBody'), {
      ok: t('schedules.detail.cancel'), consequences: [t('schedules.detail.cancelC1'), t('schedules.detail.cancelC2')],
    }))) return;
    await doPatch({ state: 'canceled' }, t('schedules.detail.canceled'));
  };

  const remove = async () => {
    if (!(await confirmDialog(t('schedules.detail.deleteTitle'), t('schedules.detail.deleteBody'), {
      ok: t('common.delete'), consequences: [t('schedules.detail.deleteC1'), t('schedules.detail.deleteC2')],
    }))) return;
    try {
      await schedules.remove(id, etag);
      forgetRecent(`#/schedules/${encodeURIComponent(id)}`);
      invalidatePalette();
      toast(t('common.deleted'), 'ok');
      navigate('/schedules');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('common.delete'));
    }
  };

  const title = () => sch.name || t('schedules.detail.untitled', { id: sch.id.slice(0, 8) });

  const renderAll = () => {
    const tz = sch.recurrence ? sch.recurrence.timezone : null;
    const active = sch.state === 'pending' || sch.state === 'running';
    replace(head, pageHeader({
      title: title(),
      badges: [schedBadge(sch.state)],
      crumbs: [{ label: t('nav.schedules'), href: '#/schedules' }, { label: title() }],
      actions: [
        active ? button(t('schedules.detail.cancel'), { icon: 'stop', onClick: cancel }) : null,
        button(t('common.delete'), { variant: 'danger', icon: 'trash', onClick: remove }),
      ].filter(Boolean),
    }));
    // いつ・何を・次はいつ（この端末の時刻と、予約のタイムゾーン）
    const now = Date.now();
    const next = occurrences(sch, now, now + 400 * 86400_000, 5);
    const nextOne = active ? next[0] : null;
    replace(hero, h('section', { class: 'sched-hero', 'data-testid': 'sched-hero' },
      h('div', { class: `sh-icon act-${sch.action}` }, icon(ACTION_ICON[sch.action] || 'clock')),
      h('div', { class: 'sh-main' },
        h('div', { class: 'sh-kicker' }, actionLabel(sch.action), ' · ', h('a', { href: `#/streams/${encodeURIComponent(sch.stream)}` }, streamName || sch.stream.slice(0, 10))),
        h('div', { class: 'sh-when' }, nextOne ? whenText(nextOne.start, nextOne.end, tz) : whenText(new Date(sch.start_at).getTime(), new Date(sch.end_at).getTime(), tz)),
        h('div', { class: 'sh-sub' },
          nextOne
            ? (nextOne.start <= now ? t('schedules.tt.now') : t('schedules.detail.nextIn', { rel: fmtRelative(new Date(nextOne.start).toISOString()) }))
            : t('schedules.detail.noMore'),
          ` · ${fmtDurationText(new Date(sch.end_at) - new Date(sch.start_at))} · ${describeRecurrence(sch.recurrence)}`)),
      active && sch.recurrence && next.length > 1 ? h('div', { class: 'sh-next' }, h('div', { class: 'small muted' }, t('schedules.detail.upcoming')),
        h('ol', null, next.slice(1).map((o) => h('li', { class: 'num' }, whenText(o.start, o.end, tz))))) : null));
    const withTz = (iso) => [fmtTime(iso), tz ? h('span', { class: 'muted small' }, ` / ${fmtTimeIn(iso, tz)}（${tz}）`) : null];
    replace(info, kvList([
      ['ID', h('code', null, sch.id)],
      [t('schedules.col.stream'), h('a', { href: `#/streams/${encodeURIComponent(sch.stream)}` }, streamName || sch.stream)],
      [t('schedules.col.action'), actionLabel(sch.action)],
      [t('schedules.col.start'), withTz(sch.start_at)],
      [t('schedules.col.end'), withTz(sch.end_at)],
      [t('schedules.detail.margins'), t('schedules.detail.marginsValue', { pre: sch.pre_roll_ms / 1000, post: sch.post_roll_ms / 1000, grace: sch.grace_ms / 1000 })],
      sch.action === 'snapshot' ? [t('schedules.form.snapInterval'), t('schedules.detail.seconds', { n: sch.snapshot_interval_secs })] : null,
      [t('schedules.col.repeat'), describeRecurrence(sch.recurrence)],
      [t('schedules.detail.retention'), sch.retention_days !== null ? t('schedules.detail.days', { n: sch.retention_days }) : t('schedules.form.retentionDefault')],
      [t('common.created'), fmtTime(sch.created_at)],
      [t('schedules.detail.generation'), String(sch.generation)],
    ]));
    if (!active) {
      replace(edit, h('p', { class: 'muted' }, t('schedules.detail.finished')));
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
      formField({ label: t('common.name'), input: name }),
      h('div', { class: 'cols' },
        formField({ label: t('schedules.detail.zone'), input: zoneSel }),
        formField({ label: t('schedules.col.start'), input: start, hint: sch.state === 'running' ? t('schedules.detail.runningStart') : null }),
        formField({ label: t('schedules.detail.endEdit'), input: end })),
      h('div', { class: 'actions' }, h('button', { type: 'button', class: 'primary', onClick: () => {
        const body = {};
        if ((sch.name || '') !== name.value) body.name = name.value || null;
        const s = wallToUtcIso(start.value, zoneSel.value);
        const e = wallToUtcIso(end.value, zoneSel.value);
        if (sch.state !== 'running' && s && new Date(s).getTime() !== new Date(sch.start_at).getTime()) body.start_at = s;
        if (e && new Date(e).getTime() !== new Date(sch.end_at).getTime()) body.end_at = e;
        if (!Object.keys(body).length) { toast(t('common.noChanges')); return; }
        doPatch(body, t('common.saved'));
      } }, t('common.save')))));
  };

  const loadRuns = () => load(runs, async () => {
    const { data } = await schedules.runs(id);
    return dataTable({
      columns: [
        { label: '#', class: 'num', render: (r) => r.occurrence },
        { label: t('schedules.col.start'), render: (r) => fmtTime(r.start_at) },
        { label: t('schedules.col.end'), render: (r) => fmtTime(r.end_at) },
        { label: t('schedules.detail.result'), render: (r) => schedBadge(r.state) },
        { label: t('nav.recordings'), render: (r) => (r.recording ? h('a', { href: `#/recordings/${encodeURIComponent(r.recording)}` }, r.recording.slice(0, 10)) : '—') },
        { label: t('schedules.detail.reason'), wrap: true, render: (r) => r.reason || '' },
      ],
      rows: data.items.slice().reverse(),
      empty: { icon: 'history', title: t('schedules.detail.noRuns'), body: t('schedules.detail.noRunsBody') },
    });
  }, { kind: 'table' });

  const reload = async () => {
    try {
      await fetchSch();
      renderAll();
      loadRuns();
    } catch (e) {
      if (e.status === 404) {
        toast(t('schedules.detail.gone'), 'warn');
        navigate('/schedules');
      } else reportError(e, t('common.refresh'));
    }
  };

  try {
    await fetchSch();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.schedules'), crumbs: [{ label: t('nav.schedules'), href: '#/schedules' }, { label: id.slice(0, 8) }] }),
      errorBox(e), h('p', null, h('a', { href: '#/schedules' }, t('common.backToList'))));
    return null;
  }
  try { streamName = (await streams.get(sch.stream)).data.name; } catch { /* 名前が出ないだけ */ }
  rememberRecent({ kind: 'schedule', id, label: title(), href: `#/schedules/${encodeURIComponent(id)}` });

  root.append(
    head,
    hero,
    h('div', { class: 'grid cols-2' },
      panel({ title: t('schedules.detail.info'), icon: 'info' }, info),
      panel({ title: t('schedules.detail.edit'), icon: 'edit' }, edit)),
    panel({ title: t('schedules.detail.runs'), icon: 'history' }, runs));
  renderAll();
  loadRuns();
  return refreshOn(['schedule.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id });
}
