// 録画の詳細: 再生（VOD）と時間軸（欠落の位置・書き出す範囲）、MP4 の書き出し（全体か範囲）、情報、名前・保持期限・法的保全、停止・削除。
//
// - 範囲は再生の位置（録画の始まりからの秒）で選び、書き出しでは started_at に足した時刻（RFC 3339）を start・end に渡す。
//   ノードは始まりを直前のキーフレームまで広げる。欠落（gaps）があると再生の位置と壁時計が少しずれることがある。
// - 書き出しは fetch のストリームで読み、進み具合を出す（中止できる）。

import { h, replace } from '../lib/dom.js';
import { confirmDialog, reportError, errorBox, toast, downloadBlob } from '../lib/ui.js';
import { recordings, streams } from '../api/resources.js';
import { fmtTime, fmtDuration, fmtDurationText, fmtBytes, utcToWall, wallToUtcIso, localZone, fmtHm, fmtClock } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { recBadge, recTitle } from './recordings.js';
import { playerWidget } from '../player/widget.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, badge, formField, toggle, helpHint, meter, kvList, tooltip } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent } from '../lib/recent.js';
import { recordingThumb } from '../lib/streaming.js';

/** ファイル名（Content-Disposition か、名前から）。 */
function fileName(res, r, range) {
  const cd = res.headers.get('content-disposition') || '';
  const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(cd);
  const base = m ? decodeURIComponent(m[1]).replace(/\.mp4$/i, '') : (r.name || r.id);
  return range ? `${base}-${range.label}.mp4` : `${base}.mp4`;
}

/**
 * MP4 を書き出してダウンロードさせる。onProgress(got, total) で進み具合。
 * @returns {Promise<number>} 書き出したバイト数
 */
export async function exportRecording(r, { signal, range, onProgress } = {}) {
  const { data: res } = await recordings.exportMp4(r.id, signal, range ? { start: range.start, end: range.end } : {});
  const total = Number(res.headers.get('content-length')) || 0;
  const reader = res.body.getReader();
  const chunks = [];
  let got = 0;
  if (!onProgress) toast(t('recordings.exporting'), 'info', 3000);
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
    got += value.byteLength;
    if (onProgress) onProgress(got, total);
  }
  downloadBlob(new Blob(chunks, { type: 'video/mp4' }), fileName(res, r, range));
  if (!onProgress) toast(t('recordings.exported', { size: fmtBytes(got) }), 'ok');
  return got;
}

export async function render(root, [id], { navigate }) {
  let rec = null;
  let etag = null;
  let exportCtrl = null;
  let streamName = null;
  // 書き出す範囲（録画の始まりからの秒）。null は全体
  let sel = { a: null, b: null };

  const head = h('div');
  const info = h('div');
  const manage = h('div');
  const exportBox = h('div');
  const timeline = h('div', { class: 'timeline', 'data-testid': 'rec-timeline' });

  const durSec = () => Math.max(0, (rec.duration_ms || 0) / 1000);

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
    const del = button(t('common.delete'), { variant: 'danger', icon: 'trash', testid: 'rec-delete', onClick: remove, attrs: { disabled: rec.legal_hold } });
    if (rec.legal_hold) tooltip(del, t('recordings.holdBlocksDelete'));
    replace(head, pageHeader({
      testid: 'rec-header',
      title: recTitle(rec),
      crumbs: [{ label: t('nav.recordings'), href: '#/recordings' }, { label: recTitle(rec) }],
      badges: [recBadge(rec), rec.legal_hold ? badge(t('recordings.legalHold'), 'accent', { icon: 'shield' }) : null],
      description: [
        h('a', { href: `#/streams/${encodeURIComponent(rec.stream)}` }, icon('streams', { size: 'sm' }), ' ', streamName || rec.stream.slice(0, 10)),
        ' · ', fmtTime(rec.started_at),
        rec.duration_ms ? ` · ${fmtDurationText(rec.duration_ms)}` : '',
        rec.bytes ? ` · ${fmtBytes(rec.bytes)}` : ''],
      actions: [
        rec.state === 'recording' ? button(t('recordings.stop'), { icon: 'stop', testid: 'rec-stop', onClick: stop }) : null,
        del,
      ].filter(Boolean),
    }));
  };

  const renderInfo = () => {
    const gaps = rec.gaps || [];
    replace(info, kvList([
      [t('recordings.col.stream'), h('a', { href: `#/streams/${encodeURIComponent(rec.stream)}` }, streamName || rec.stream)],
      [t('common.state'), recBadge(rec)],
      [t('recordings.col.started'), fmtTime(rec.started_at)],
      [t('recordings.ended'), rec.ended_at ? fmtTime(rec.ended_at) : '—'],
      [t('recordings.col.duration'), rec.duration_ms ? [fmtDuration(rec.duration_ms), ' ', h('span', { class: 'muted small' }, fmtDurationText(rec.duration_ms))] : '—'],
      [t('recordings.col.size'), fmtBytes(rec.bytes)],
      [t('recordings.gaps'), gaps.length
        ? h('ul', { class: 'gap-list' }, gaps.map((g) => h('li', null, h('button', { type: 'button', class: 'link', onClick: () => seekWall(g.at) }, fmtHm(g.at)), ' ', h('code', null, g.reason), g.dropped ? ` · ${t('recordings.gapDroppedN', { n: g.dropped })}` : '')))
        : h('span', { class: 'text-ok' }, icon('success', { size: 'sm' }), ' ', t('recordings.noGaps'))],
      [t('recordings.storage'), rec.storage ? h('span', { title: rec.storage.uri }, icon('storage', { size: 'sm' }), ' ', t('recordings.archived', { time: fmtTime(rec.storage.archived_at) })) : t('recordings.localOnly')],
      rec.schedule ? [t('recordings.schedule'), h('a', { href: `#/schedules/${encodeURIComponent(rec.schedule)}` }, t('recordings.fromSchedule'))] : null,
      ['ID', h('code', null, rec.id)],
    ]));
  };

  const renderManage = () => {
    const name = h('input', { type: 'text', maxlength: 128, value: rec.name || '', placeholder: t('recordings.namePh') });
    const zone = localZone();
    const until = h('input', { type: 'datetime-local', value: rec.retention_until ? utcToWall(rec.retention_until, zone) : '' });
    const forever = h('input', { type: 'checkbox', checked: !rec.retention_until });
    const syncUntil = () => { until.disabled = forever.checked; };
    forever.addEventListener('change', syncUntil);
    syncUntil();
    const hold = toggle({ label: t('recordings.legalHoldLabel'), description: t('recordings.holdHint'), checked: rec.legal_hold, testid: 'rec-hold' });
    hold.input.addEventListener('change', async () => {
      const want = hold.input.checked;
      if (!want && !(await confirmDialog(t('recordings.unholdTitle'), t('recordings.unholdBody'), { ok: t('recordings.unholdOk'), consequences: [t('recordings.unholdC1')] }))) {
        hold.input.checked = true;
        return;
      }
      await patchRec({ legal_hold: want }, want ? t('recordings.held') : t('recordings.unheld'));
    });
    const untilField = formField({ label: t('recordings.retentionUntil', { zone }), input: until, hint: t('recordings.retentionHint') });
    until.after(h('label', { class: 'check small' }, forever, t('recordings.keepForever')));
    replace(manage, h('div', { class: 'form' },
      h('div', { class: 'hold-box' }, hold.el),
      formField({ label: t('common.name'), input: name }),
      untilField,
      h('div', { class: 'actions' }, button(t('common.save'), { icon: 'save', testid: 'rec-save', onClick: async () => {
        const body = {};
        if ((rec.name || '') !== name.value) body.name = name.value;
        const ret = forever.checked ? null : wallToUtcIso(until.value, zone);
        if (!forever.checked && !ret) { untilField.setError(t('recordings.needUntil')); until.focus(); return; }
        untilField.setError(null);
        if (ret !== rec.retention_until && !(ret && rec.retention_until && new Date(ret).getTime() === new Date(rec.retention_until).getTime())) body.retention_until = ret;
        if (!Object.keys(body).length) { toast(t('common.noChanges'), 'info'); return; }
        await patchRec(body, t('common.saved'));
      } }))));
  };

  // ---------------------------------------------------------------- 時間軸と範囲
  let player = null;
  const video = () => player && player.video;
  const seek = (sec) => {
    const v = video();
    if (!v) return;
    if (!player.playing) {
      player.start().then(() => { const w = video(); if (w) w.currentTime = sec; });
      return;
    }
    v.currentTime = sec;
  };
  const seekWall = (iso) => seek(Math.max(0, (new Date(iso).getTime() - new Date(rec.started_at).getTime()) / 1000));
  const pct = (sec) => `${Math.max(0, Math.min(100, (sec / Math.max(durSec(), 0.001)) * 100)).toFixed(2)}%`;
  const selRange = () => {
    const a = sel.a ?? 0;
    const b = sel.b ?? durSec();
    if (sel.a === null && sel.b === null) return null;
    if (!(b > a)) return null;
    const startMs = new Date(rec.started_at).getTime();
    return { a, b, start: new Date(startMs + a * 1000).toISOString(), end: new Date(startMs + b * 1000).toISOString(), label: `${fmtDuration(a * 1000).replace(/:/g, '')}-${fmtDuration(b * 1000).replace(/:/g, '')}` };
  };

  const renderTimeline = () => {
    const d = durSec();
    if (!d || rec.state === 'recording') {
      replace(timeline, h('p', { class: 'hint' }, rec.state === 'recording' ? t('recordings.tl.recording') : t('recordings.tl.empty')));
      return;
    }
    const startMs = new Date(rec.started_at).getTime();
    const track = h('div', { class: 'tl-track', role: 'presentation' });
    const r = selRange();
    if (r) {
      const s = h('div', { class: 'tl-sel' });
      s.style.left = pct(r.a);
      s.style.width = `calc(${pct(r.b)} - ${pct(r.a)})`;
      track.append(s);
    }
    for (const g of rec.gaps || []) {
      const at = (new Date(g.at).getTime() - startMs) / 1000;
      const m = h('button', { type: 'button', class: 'tl-gap', 'aria-label': t('recordings.tl.gapAt', { time: fmtHm(g.at), reason: g.reason }), onClick: (e) => { e.stopPropagation(); seek(at); } });
      m.style.left = pct(at);
      tooltip(m, t('recordings.tl.gapAt', { time: fmtHm(g.at), reason: g.reason }));
      track.append(m);
    }
    const headEl = h('div', { class: 'tl-head', 'aria-hidden': 'true' });
    track.append(headEl);
    track.addEventListener('click', (e) => {
      const b = track.getBoundingClientRect();
      seek(((e.clientX - b.left) / b.width) * d);
    });
    const step = d > 600 ? 5 : 1;
    const rangeInput = (which) => {
      const input = h('input', { type: 'range', min: 0, max: Math.floor(d), step, value: which === 'a' ? sel.a ?? 0 : sel.b ?? Math.floor(d), 'aria-label': which === 'a' ? t('recordings.tl.start') : t('recordings.tl.end'), 'data-testid': `rec-range-${which}` });
      input.addEventListener('input', () => {
        sel[which] = Number(input.value);
        if (sel.a !== null && sel.b !== null && sel.b <= sel.a) { if (which === 'a') sel.b = Math.min(Math.floor(d), sel.a + step); else sel.a = Math.max(0, sel.b - step); }
        renderTimeline();
        renderExport();
        timeline.querySelector(`[data-testid="rec-range-${which}"]`)?.focus();
      });
      return input;
    };
    const fromPlayer = (which) => button(which === 'a' ? t('recordings.tl.markIn') : t('recordings.tl.markOut'), {
      size: 'sm', icon: which === 'a' ? 'chevron-right' : 'chevron-left', testid: `rec-mark-${which}`,
      onClick: () => {
        const v = video();
        const now = v ? v.currentTime : 0;
        sel[which] = Math.round(now);
        if (sel.a !== null && sel.b !== null && sel.b <= sel.a) sel[which === 'a' ? 'b' : 'a'] = null;
        renderTimeline();
        renderExport();
      },
    });
    const sr = selRange();
    replace(timeline,
      track,
      h('div', { class: 'tl-scale small muted num' }, h('span', null, fmtClock(rec.started_at)), h('span', null, fmtDuration(d * 1000)), h('span', null, rec.ended_at ? fmtClock(rec.ended_at) : '')),
      h('div', { class: 'tl-ranges' },
        h('label', { class: 'tl-range' }, h('span', null, t('recordings.tl.start'), ' ', h('strong', { class: 'num' }, fmtDuration((sel.a ?? 0) * 1000))), rangeInput('a')),
        h('label', { class: 'tl-range' }, h('span', null, t('recordings.tl.end'), ' ', h('strong', { class: 'num' }, fmtDuration((sel.b ?? d) * 1000))), rangeInput('b'))),
      h('div', { class: 'tl-actions' },
        fromPlayer('a'), fromPlayer('b'),
        sr ? button(t('recordings.tl.clear'), { size: 'sm', variant: 'ghost', icon: 'close', testid: 'rec-range-clear', onClick: () => { sel = { a: null, b: null }; renderTimeline(); renderExport(); } }) : null,
        h('span', { class: 'hint tl-sum', 'data-testid': 'rec-range-summary' }, sr
          ? t('recordings.tl.selected', { from: fmtClock(sr.start), to: fmtClock(sr.end), dur: fmtDurationText((sr.b - sr.a) * 1000) })
          : t('recordings.tl.whole'))));
    // 再生の位置を時間軸に出す
    const v = video();
    if (v && !v.dataset.tl) {
      v.dataset.tl = '1';
      v.addEventListener('timeupdate', () => { const hd = timeline.querySelector('.tl-head'); if (hd) hd.style.left = pct(v.currentTime); });
    }
  };

  const renderExport = () => {
    const recording = rec.state === 'recording';
    const r = selRange();
    const btn = button(r ? t('recordings.exportRange') : t('recordings.export'), { variant: 'primary', icon: 'download', testid: 'rec-export', attrs: { disabled: recording || !rec.bytes } });
    const cancel = button(t('recordings.exportCancel'), { attrs: { disabled: true } });
    const bar = h('div', { class: 'export-bar' });
    const msg = h('span', { class: 'small muted', role: 'status' }, recording ? t('recordings.exportWhileRecording') : r ? t('recordings.exportRangeHint') : t('recordings.exportHint'));
    const err = h('div', { class: 'export-err' });
    const setBar = (value) => replace(bar, meter({ label: t('recordings.exportProgress'), value, valueText: value === null ? '…' : undefined }));
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      cancel.disabled = false;
      exportCtrl = new AbortController();
      msg.textContent = t('recordings.exporting');
      replace(err);
      setBar(null);
      try {
        const got = await exportRecording(rec, {
          signal: exportCtrl.signal, range: r,
          onProgress: (n, total) => { if (total) setBar(n / total); msg.textContent = `${fmtBytes(n)}${total ? ` / ${fmtBytes(total)}` : ''}`; },
        });
        msg.textContent = t('recordings.exported', { size: fmtBytes(got) });
        setBar(1);
      } catch (e) {
        replace(bar);
        if (e.name === 'AbortError') msg.textContent = t('recordings.exportCancelled');
        else {
          msg.textContent = '';
          replace(err, errorBox(e, { title: t('recordings.exportFailed') }));
        }
      } finally {
        btn.disabled = rec.state === 'recording';
        cancel.disabled = true;
        exportCtrl = null;
      }
    });
    cancel.addEventListener('click', () => exportCtrl && exportCtrl.abort());
    replace(exportBox, h('div', { class: 'export-row' }, btn, cancel, bar), h('div', { class: 'mt-2' }, msg), err);
  };

  const renderAll = () => {
    renderHead();
    renderInfo();
    renderManage();
    renderTimeline();
  };

  const reload = async () => {
    try {
      const wasState = rec && rec.state;
      const wasGen = rec && rec.generation;
      await fetchRec();
      renderHead();
      renderInfo();
      renderTimeline();
      // 名前・保持の欄は、内容が変わったときだけ作り直す（入力中の文字を消さない）
      if (wasGen !== rec.generation) renderManage();
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
      errorBox(e, { actions: [h('a', { class: 'btn sm', href: '#/recordings' }, icon('arrow-left'), t('common.backToList'))] }));
    return null;
  }
  try { streamName = (await streams.get(rec.stream)).data.name; } catch { /* 削除済みの配信 */ }
  rememberRecent({ kind: 'recording', id, label: recTitle(rec), href: `#/recordings/${encodeURIComponent(id)}` });

  player = playerWidget({
    getUrl: async () => (await recordings.playbackUrl(id)).data.playback.hls,
    live: false,
    label: t('recordings.playerLabel'),
    poster: () => (rec.thumbnail ? recordingThumb(id) : Promise.resolve(null)),
  });

  root.append(
    head,
    h('div', { class: 'rec-layout' },
      h('div', { class: 'rec-main' },
        panel({ title: t('recordings.vod'), icon: 'play' }, player.el, h('p', { class: 'small muted mt-2' }, t('recordings.vodNote'))),
        panel({ title: t('recordings.exportTitle'), icon: 'download', description: t('recordings.exportDesc'), help: helpHint({ title: t('recordings.exportTitle'), body: t('recordings.exportHelp'), guide: '05-recording.md' }) }, timeline, h('div', { class: 'mt-4' }, exportBox))),
      h('div', { class: 'rec-side' },
        panel({ title: t('recordings.info'), icon: 'info' }, info),
        panel({ title: t('recordings.manage'), icon: 'shield', help: helpHint({ title: t('recordings.legalHoldLabel'), body: t('recordings.holdHelp'), guide: '05-recording.md' }) }, manage))));
  renderAll();
  renderExport();

  const off = refreshOn(['recording.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id });
  // 録画中は長さ・大きさが増えるので読み直す
  const timer = setInterval(() => { if (rec.state === 'recording' && document.visibilityState === 'visible') reload(); }, 5000);
  return () => {
    off();
    clearInterval(timer);
    player.stop();
    if (exportCtrl) exportCtrl.abort();
  };
}

