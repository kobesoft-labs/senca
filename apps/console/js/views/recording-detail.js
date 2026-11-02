// 録画の詳細: 状態・欠落・代表画・法的保全・保持期限・名前・VOD 再生・MP4 書き出し・停止・削除。

import { h, replace, table, kv, field, badge } from '../lib/dom.js';
import { confirmDialog, reportError, errorBox, toast, downloadBlob, describeError } from '../lib/ui.js';
import { recordings, streams } from '../api/resources.js';
import { fmtTime, fmtDuration, fmtBytes, utcToWall, wallToUtcIso, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { recBadge } from './recordings.js';
import { playerWidget } from '../player/widget.js';

export async function render(root, [id], { navigate }) {
  let rec = null;
  let etag = null;
  let thumbUrl = null;
  let exportCtrl = null;
  let streamName = null;

  const head = h('div', { class: 'page-head' });
  const info = h('div');
  const gaps = h('div');
  const thumb = h('div');
  const manage = h('div');
  const exportBox = h('div');

  const fetchRec = async () => {
    const r = await recordings.get(id);
    rec = r.data;
    etag = r.etag;
  };

  const on412 = async (e) => {
    if (e.status !== 412) return false;
    await reload();
    toast('他の操作で更新されていたので、最新の内容を読み込み直しました。もう一度操作してください', 'warn', 8000);
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
      if (!(await on412(e))) reportError(e, '更新');
      else renderAll();
    }
  };

  const renderHead = () => {
    replace(head,
      h('h1', null, rec.name || `録画 ${fmtTime(rec.started_at)}`), recBadge(rec),
      rec.legal_hold ? badge('法的保全', 'danger') : null,
      h('span', { class: 'spacer' }),
      rec.state === 'recording' ? h('button', { onClick: stop }, '■ 録画を止める') : null,
      h('button', { class: 'danger', onClick: remove, disabled: rec.legal_hold, title: rec.legal_hold ? '法的保全中は削除できません' : null }, '削除'));
  };

  const renderInfo = () => {
    replace(info, kv([
      ['ID', h('code', null, rec.id)],
      ['配信', h('a', { href: `#/streams/${encodeURIComponent(rec.stream)}` }, streamName || rec.stream)],
      ['状態', recBadge(rec)],
      ['開始', fmtTime(rec.started_at)],
      ['終了', rec.ended_at ? fmtTime(rec.ended_at) : '—'],
      ['長さ', fmtDuration(rec.duration_ms)],
      ['セグメント', rec.segments ?? '—'],
      ['大きさ', fmtBytes(rec.bytes)],
      ['保持期限', rec.retention_until ? fmtTime(rec.retention_until) : '無期限'],
      ['予約', rec.schedule ? h('a', { href: `#/schedules/${encodeURIComponent(rec.schedule)}` }, rec.schedule.slice(0, 10)) : '—'],
      ['世代', String(rec.generation)],
    ]));
    replace(gaps, table([
      { label: '時刻', render: (g) => fmtTime(g.at) },
      { label: '理由', render: (g) => h('code', null, g.reason) },
      { label: '捨てた数', class: 'num', render: (g) => g.dropped },
    ], rec.gaps || [], { empty: '欠落はありません' }));
  };

  const renderManage = () => {
    const name = h('input', { type: 'text', maxlength: 128, value: rec.name || '' });
    const zone = localZone();
    const until = h('input', { type: 'datetime-local', value: rec.retention_until ? utcToWall(rec.retention_until, zone) : '' });
    const forever = h('input', { type: 'checkbox', checked: !rec.retention_until });
    const syncUntil = () => { until.disabled = forever.checked; };
    forever.addEventListener('change', syncUntil);
    syncUntil();
    const hold = h('input', { type: 'checkbox', role: 'switch', checked: rec.legal_hold, 'aria-describedby': 'hold-hint' });
    hold.addEventListener('change', async () => {
      const want = hold.checked;
      if (!want && !(await confirmDialog('法的保全を解除', '解除すると、保持期限を過ぎた録画や削除の操作で消えるようになります。解除には recording:delete の権限が要ります。', { ok: '解除' }))) {
        hold.checked = true;
        return;
      }
      await patchRec({ legal_hold: want }, want ? '法的保全にしました' : '法的保全を解除しました');
    });
    replace(manage, h('div', { class: 'form' },
      h('div', { class: 'field' },
        h('label', { class: 'check' }, hold, h('strong', null, '法的保全（legal hold）')),
        h('div', { class: 'hint', id: 'hold-hint' }, '保全中は削除・期限切れの回収をしません。')),
      field('名前', name),
      h('div', { class: 'field' }, h('label', { for: 'ret-until' }, `保持期限（${zone}）`),
        h('div', { class: 'inline' }, Object.assign(until, { id: 'ret-until' }), h('label', { class: 'check' }, forever, '無期限'))),
      h('div', { class: 'actions' }, h('button', { class: 'primary', onClick: async () => {
        const body = {};
        if ((rec.name || '') !== name.value) body.name = name.value;
        const ret = forever.checked ? null : wallToUtcIso(until.value, zone);
        if (!forever.checked && !ret) { toast('保持期限の日時を入れてください', 'warn'); return; }
        if (ret !== rec.retention_until && !(ret && rec.retention_until && new Date(ret).getTime() === new Date(rec.retention_until).getTime())) body.retention_until = ret;
        if (!Object.keys(body).length) { toast('変更はありません', 'info'); return; }
        await patchRec(body, '保存しました');
      } }, '保存'))));
  };

  const renderThumb = async () => {
    if (!rec.thumbnail) {
      replace(thumb, h('div', { class: 'muted' }, rec.state === 'recording' ? '代表画は録画の確定後に作ります。' : '代表画はありません。'));
      return;
    }
    try {
      const { data } = await recordings.thumbnail(id);
      if (thumbUrl) URL.revokeObjectURL(thumbUrl);
      thumbUrl = URL.createObjectURL(data);
      replace(thumb, h('img', { class: 'snapshot', src: thumbUrl, alt: '録画の代表画' }));
    } catch (e) {
      replace(thumb, h('div', { class: 'muted' }, `代表画を読めません: ${describeError(e)}`));
    }
  };

  const renderExport = () => {
    const btn = h('button', { class: 'primary', disabled: rec.state === 'recording' }, 'MP4 を書き出す');
    const cancel = h('button', { disabled: true }, '中止');
    const progress = h('progress', { max: 1, value: 0, style: { width: '200px' }, 'aria-label': '書き出しの進み具合' });
    const msg = h('span', { class: 'small muted', role: 'status' }, rec.state === 'recording' ? '録画中は書き出せません。' : '');
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      cancel.disabled = false;
      exportCtrl = new AbortController();
      msg.textContent = '書き出し中…';
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
          if (total) progress.value = got / total;
          msg.textContent = `${fmtBytes(got)}${total ? ` / ${fmtBytes(total)}` : ''}`;
        }
        const cd = res.headers.get('content-disposition') || '';
        const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(cd);
        const fname = m ? decodeURIComponent(m[1]) : `${rec.name || rec.id}.mp4`;
        downloadBlob(new Blob(chunks, { type: 'video/mp4' }), fname);
        msg.textContent = `書き出しました（${fmtBytes(got)}）`;
        progress.value = 1;
      } catch (e) {
        if (e.name === 'AbortError') msg.textContent = '中止しました';
        else {
          msg.textContent = '';
          replace(exportBox.querySelector('.export-err'), errorBox(e, { title: '書き出せませんでした' }));
        }
      } finally {
        btn.disabled = rec.state === 'recording';
        cancel.disabled = true;
        exportCtrl = null;
      }
    });
    cancel.addEventListener('click', () => exportCtrl && exportCtrl.abort());
    replace(exportBox, h('div', { class: 'row' }, btn, cancel, progress, msg), h('div', { class: 'export-err' }));
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
        toast('この録画は削除されました', 'warn');
        navigate('/recordings');
      } else reportError(e, '読み込み');
    }
  };

  const stop = async () => {
    try {
      await recordings.stop(id);
      toast('停止を受け付けました（確定まで少しかかります）', 'ok');
      setTimeout(reload, 1500);
    } catch (e) {
      reportError(e, '停止');
    }
  };

  const remove = async () => {
    const ok = await confirmDialog('録画を削除', `この録画（${fmtDuration(rec.duration_ms)}、${fmtBytes(rec.bytes)}）を削除します。元に戻せません。${rec.state === 'recording' ? '録画中なので止めてから消します。' : ''}`, { ok: '削除', requireText: '削除' });
    if (!ok) return;
    try {
      await recordings.remove(id, etag);
      toast('削除を受け付けました', 'ok');
      navigate('/recordings');
    } catch (e) {
      if (!(await on412(e))) reportError(e, '削除');
    }
  };

  try {
    await fetchRec();
  } catch (e) {
    root.append(h('h1', null, '録画'), errorBox(e), h('p', null, h('a', { href: '#/recordings' }, '一覧へ戻る')));
    return null;
  }
  try { streamName = (await streams.get(rec.stream)).data.name; } catch { /* 削除済みの配信 */ }

  const player = playerWidget({
    getUrl: async () => (await recordings.playbackUrl(id)).data.playback.hls,
    live: false,
    label: '録画の再生',
  });

  root.append(
    h('nav', { class: 'small', 'aria-label': 'パンくず' }, h('a', { href: '#/recordings' }, '録画'), ' / ', rec.id.slice(0, 10)),
    head,
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 'r-pv' }, h('h2', { id: 'r-pv' }, '再生（VOD）'), player.el,
        h('p', { class: 'small muted' }, 'fMP4 の録画はこの画面で再生できます。MPEG-TS の録画は Safari か、MP4 に書き出して再生してください。')),
      h('section', { class: 'panel', 'aria-labelledby': 'r-in' }, h('h2', { id: 'r-in' }, '情報'), info)),
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 'r-mg' }, h('h2', { id: 'r-mg' }, '保持と保全'), manage),
      h('section', { class: 'panel', 'aria-labelledby': 'r-th' }, h('h2', { id: 'r-th' }, '代表画'), thumb)),
    h('section', { class: 'panel', 'aria-labelledby': 'r-gp' }, h('h2', { id: 'r-gp' }, '欠落区間'), gaps),
    h('section', { class: 'panel', 'aria-labelledby': 'r-ex' }, h('h2', { id: 'r-ex' }, 'MP4 の書き出し'), exportBox));
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
