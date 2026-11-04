// 録画の詳細: 状態・欠落・代表画・法的保全・保持期限・名前・VOD 再生・MP4 書き出し・停止・削除。

import { h, replace, kv } from '../lib/dom.js';
import { confirmDialog, reportError, errorBox, toast, downloadBlob, describeError } from '../lib/ui.js';
import { recordings, streams } from '../api/resources.js';
import { fmtTime, fmtDuration, fmtDurationText, fmtBytes, utcToWall, wallToUtcIso, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { recBadge } from './recordings.js';
import { playerWidget } from '../player/widget.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, dataTable, badge, formField, toggle, helpHint, emptyState, meter } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent } from '../lib/recent.js';

export async function render(root, [id], { navigate }) {
  let rec = null;
  let etag = null;
  let thumbUrl = null;
  let exportCtrl = null;
  let streamName = null;

  const head = h('div');
  const info = h('div');
  const gaps = h('div');
  const thumb = h('div');
  const manage = h('div');
  const exportBox = h('div');

  const recTitle = () => rec.name || t('recordings.titleAt', { time: fmtTime(rec.started_at) });

  const fetchRec = async () => {
    const r = await recordings.get(id);
    rec = r.data;
    etag = r.etag;
  };

  const on412 = async (e) => {
    if (e.status !== 412) return false;
    await reload();
    toast(t('recordings.stale'), 'warn', 8000);
    return true;
  };

  const patchRec = async (body, okText) => {
    try {
      const r = await recordings.update(id, body, etag);
      rec = r.data;
      etag = r.etag;
      renderAll();
      toast(okText, 'ok');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('recordings.update'));
      else renderAll();
    }
  };

  const renderHead = () => {
    replace(head, pageHeader({
      title: recTitle(),
      crumbs: [{ label: t('nav.recordings'), href: '#/recordings' }, { label: recTitle() }],
      badges: [recBadge(rec), rec.legal_hold ? badge(t('recordings.legalHold'), 'accent', { icon: 'shield' }) : null],
      actions: [
        rec.state === 'recording' ? button(t('recordings.stop'), { icon: 'stop', onClick: stop }) : null,
        button(t('common.delete'), { variant: 'danger', icon: 'trash', onClick: remove, attrs: { disabled: rec.legal_hold, title: rec.legal_hold ? t('recordings.holdBlocksDelete') : null } }),
      ].filter(Boolean),
    }));
  };

  const renderInfo = () => {
    replace(info, kv([
      ['ID', h('code', null, rec.id)],
      [t('recordings.col.stream'), h('a', { href: `#/streams/${encodeURIComponent(rec.stream)}` }, streamName || rec.stream)],
      [t('common.state'), recBadge(rec)],
      [t('recordings.col.started'), fmtTime(rec.started_at)],
      [t('recordings.ended'), rec.ended_at ? fmtTime(rec.ended_at) : '—'],
      [t('recordings.col.duration'), rec.duration_ms ? [fmtDuration(rec.duration_ms), ' ', h('span', { class: 'muted small' }, fmtDurationText(rec.duration_ms))] : fmtDuration(rec.duration_ms)],
      [t('recordings.segments'), rec.segments ?? '—'],
      [t('recordings.col.size'), fmtBytes(rec.bytes)],
      [t('recordings.col.retention'), rec.retention_until ? fmtTime(rec.retention_until) : t('common.unlimited')],
      [t('recordings.schedule'), rec.schedule ? h('a', { href: `#/schedules/${encodeURIComponent(rec.schedule)}` }, rec.schedule.slice(0, 10)) : '—'],
      [t('recordings.generation'), String(rec.generation)],
    ]));
    replace(gaps, dataTable({
      columns: [
        { label: t('recordings.gapAt'), primary: true, render: (g) => fmtTime(g.at) },
        { label: t('recordings.gapReason'), render: (g) => h('code', null, g.reason) },
        { label: t('recordings.gapDropped'), class: 'num', render: (g) => g.dropped },
      ],
      rows: rec.gaps || [],
      empty: { icon: 'success', title: t('recordings.noGaps'), body: t('recordings.noGapsBody') },
    }));
  };

  const renderManage = () => {
    const name = h('input', { type: 'text', maxlength: 128, value: rec.name || '' });
    const zone = localZone();
    const until = h('input', { type: 'datetime-local', id: 'ret-until', value: rec.retention_until ? utcToWall(rec.retention_until, zone) : '' });
    const forever = h('input', { type: 'checkbox', checked: !rec.retention_until });
    const syncUntil = () => { until.disabled = forever.checked; };
    forever.addEventListener('change', syncUntil);
    syncUntil();
    const hold = toggle({ label: t('recordings.legalHoldLabel'), description: t('recordings.holdHint'), checked: rec.legal_hold });
    hold.input.addEventListener('change', async () => {
      const want = hold.input.checked;
      if (!want && !(await confirmDialog(t('recordings.unholdTitle'), t('recordings.unholdBody'), { ok: t('recordings.unholdOk'), consequences: [t('recordings.unholdC1')] }))) {
        hold.input.checked = true;
        return;
      }
      await patchRec({ legal_hold: want }, want ? t('recordings.held') : t('recordings.unheld'));
    });
    const untilField = formField({ label: t('recordings.retentionUntil', { zone }), input: until });
    until.after(h('label', { class: 'check small' }, forever, t('common.unlimited')));
    replace(manage, h('div', { class: 'form' },
      hold.el,
      formField({ label: t('common.name'), input: name }),
      untilField,
      h('div', { class: 'actions' }, h('button', { type: 'button', class: 'primary', onClick: async () => {
        const body = {};
        if ((rec.name || '') !== name.value) body.name = name.value;
        const ret = forever.checked ? null : wallToUtcIso(until.value, zone);
        if (!forever.checked && !ret) { untilField.setError(t('recordings.needUntil')); until.focus(); return; }
        untilField.setError(null);
        if (ret !== rec.retention_until && !(ret && rec.retention_until && new Date(ret).getTime() === new Date(rec.retention_until).getTime())) body.retention_until = ret;
        if (!Object.keys(body).length) { toast(t('common.noChanges'), 'info'); return; }
        await patchRec(body, t('common.saved'));
      } }, icon('save'), h('span', null, t('common.save'))))));
  };

  const renderThumb = async () => {
    if (!rec.thumbnail) {
      replace(thumb, emptyState({ compact: true, icon: 'image', title: rec.state === 'recording' ? t('recordings.thumbLater') : t('recordings.noThumb') }));
      return;
    }
    try {
      const { data } = await recordings.thumbnail(id);
      if (thumbUrl) URL.revokeObjectURL(thumbUrl);
      thumbUrl = URL.createObjectURL(data);
      replace(thumb, h('img', { class: 'snapshot', src: thumbUrl, alt: t('recordings.thumbAlt') }));
    } catch (e) {
      replace(thumb, h('div', { class: 'muted' }, t('recordings.thumbFailed', { reason: describeError(e) })));
    }
  };

  const renderExport = () => {
    const recording = rec.state === 'recording';
    const btn = h('button', { type: 'button', class: 'primary', disabled: recording }, icon('download'), h('span', null, t('recordings.export')));
    const cancel = h('button', { type: 'button', disabled: true }, t('recordings.exportCancel'));
    const bar = h('div');
    bar.style.flex = '1 1 200px';
    const msg = h('span', { class: 'small muted', role: 'status' }, recording ? t('recordings.exportWhileRecording') : t('recordings.exportHint'));
    const setBar = (value) => replace(bar, meter({ label: t('recordings.exportProgress'), value, valueText: value === null ? '…' : undefined }));
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      cancel.disabled = false;
      exportCtrl = new AbortController();
      msg.textContent = t('recordings.exporting');
      setBar(null);
      try {
        const { data: res } = await recordings.exportMp4(id, exportCtrl.signal);
        const total = Number(res.headers.get('content-length')) || 0;
        const reader = res.body.getReader();
        const chunks = [];
        let got = 0;
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          chunks.push(value);
          got += value.byteLength;
          if (total) setBar(got / total);
          msg.textContent = `${fmtBytes(got)}${total ? ` / ${fmtBytes(total)}` : ''}`;
        }
        const cd = res.headers.get('content-disposition') || '';
        const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(cd);
        const fname = m ? decodeURIComponent(m[1]) : `${rec.name || rec.id}.mp4`;
        downloadBlob(new Blob(chunks, { type: 'video/mp4' }), fname);
        msg.textContent = t('recordings.exported', { size: fmtBytes(got) });
        setBar(1);
      } catch (e) {
        replace(bar);
        if (e.name === 'AbortError') msg.textContent = t('recordings.exportCancelled');
        else {
          msg.textContent = '';
          replace(exportBox.querySelector('.export-err'), errorBox(e, { title: t('recordings.exportFailed') }));
        }
      } finally {
        btn.disabled = rec.state === 'recording';
        cancel.disabled = true;
        exportCtrl = null;
      }
    });
    cancel.addEventListener('click', () => exportCtrl && exportCtrl.abort());
    replace(exportBox, h('div', { class: 'row' }, btn, cancel, bar), h('div', { class: 'mt-2' }, msg), h('div', { class: 'export-err mt-2' }));
  };

  const renderAll = () => {
    renderHead();
    renderInfo();
    renderManage();
  };

  const reload = async () => {
    try {
      const wasThumb = rec && rec.thumbnail;
      const wasState = rec && rec.state;
      await fetchRec();
      renderAll();
      if (!wasThumb && rec.thumbnail) renderThumb();
      if (wasState !== rec.state) renderExport();
    } catch (e) {
      if (e.status === 404) {
        toast(t('recordings.gone'), 'warn');
        navigate('/recordings');
      } else reportError(e, t('recordings.reload'));
    }
  };

  const stop = async () => {
    try {
      await recordings.stop(id);
      toast(t('recordings.stopAccepted'), 'ok');
      setTimeout(reload, 1500);
    } catch (e) {
      reportError(e, t('recordings.stopShort'));
    }
  };

  const remove = async () => {
    const consequences = [t('recordings.deleteC1')];
    if (rec.state === 'recording') consequences.push(t('recordings.deleteC2'));
    const ok = await confirmDialog(t('recordings.deleteTitle'),
      t('recordings.deleteBody', { duration: fmtDuration(rec.duration_ms), size: fmtBytes(rec.bytes) }),
      { ok: t('common.delete'), requireText: t('common.delete'), consequences });
    if (!ok) return;
    try {
      await recordings.remove(id, etag);
      toast(t('recordings.deleteAccepted'), 'ok');
      navigate('/recordings');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('common.delete'));
    }
  };

  try {
    await fetchRec();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.recordings'), crumbs: [{ label: t('nav.recordings'), href: '#/recordings' }, { label: id.slice(0, 10) }] }),
      errorBox(e), h('p', null, h('a', { href: '#/recordings' }, t('common.backToList'))));
    return null;
  }
  try { streamName = (await streams.get(rec.stream)).data.name; } catch { /* 削除済みの配信 */ }
  rememberRecent({ kind: 'recording', id, label: recTitle(), href: `#/recordings/${encodeURIComponent(id)}` });

  const player = playerWidget({
    getUrl: async () => (await recordings.playbackUrl(id)).data.playback.hls,
    live: false,
    label: t('recordings.playerLabel'),
  });

  root.append(
    head,
    h('div', { class: 'grid cols-2' },
      panel({ title: t('recordings.vod'), icon: 'play' }, player.el,
        h('p', { class: 'small muted mt-2' }, t('recordings.vodNote'))),
      panel({ title: t('recordings.info'), icon: 'info' }, info)),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('recordings.manage'), icon: 'shield', help: helpHint({ title: t('recordings.legalHoldLabel'), body: t('recordings.holdHelp'), guide: '05-recording.md' }) }, manage),
      panel({ title: t('recordings.thumb'), icon: 'image' }, thumb)),
    h('div', { class: 'section-gap' }),
    panel({ title: t('recordings.gaps'), icon: 'warning', description: t('recordings.gapsDesc') }, gaps),
    panel({ title: t('recordings.exportTitle'), icon: 'download' }, exportBox));
  renderAll();
  renderThumb();
  renderExport();

  const off = refreshOn(['recording.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id });
  return () => {
    off();
    player.stop();
    if (exportCtrl) exportCtrl.abort();
    if (thumbUrl) URL.revokeObjectURL(thumbUrl);
  };
}
