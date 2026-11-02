// 配信の詳細: 状態・取り込み先・プレビュー・今の画・保存した静止画・変換計画・編集・削除・鍵の作り直し。

import { h, replace, table, kv, badge } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, toast, copyField, describeError } from '../lib/ui.js';
import { streams, recordings } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtBps, fmtBytes, fmtDuration } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { streamForm } from './stream-form.js';
import { stateBadge } from './streams.js';
import { recBadge } from './recordings.js';
import { takeSecret } from '../lib/secrets.js';
import { playerWidget } from '../player/widget.js';

export async function render(root, [id], { caps, navigate }) {
  let stream = null;
  let etag = null;
  let secret = takeSecret(id);
  const blobUrls = new Set();
  const timers = [];

  const head = h('div', { class: 'page-head' });
  const overview = h('div');
  const ingest = h('div');
  const snapNow = h('div');
  const gallery = h('div');
  const planBox = h('div', { class: 'muted' }, '「計画を取る」で、入力のコーデックに対する出力ごとの扱い（そのまま・再多重化・変換・不可）を確かめます。');
  const recBox = h('div');

  const player = playerWidget({ getUrl: async () => (await streams.playbackUrl(id)).data.playback.hls, live: true, label: 'ライブのプレビュー' });

  const blobUrl = (blob) => {
    const u = URL.createObjectURL(blob);
    blobUrls.add(u);
    return u;
  };

  const fetchStream = async () => {
    const r = await streams.get(id);
    stream = r.data;
    etag = r.etag;
    return stream;
  };

  const renderHead = () => {
    const toggle = stream.desired_state === 'running'
      ? h('button', { onClick: () => setDesired('stopped') }, '停止（publish を拒否）')
      : h('button', { onClick: () => setDesired('running') }, '再開');
    replace(head,
      h('h1', null, stream.name), stateBadge(stream), h('span', { class: 'spacer' }),
      h('button', { onClick: () => startRecording(), disabled: stream.observed_state !== 'live' && stream.desired_state === 'stopped' }, '● 録画を開始'),
      toggle,
      h('button', { onClick: () => edit() }, '編集'),
      h('button', { class: 'danger', onClick: () => remove() }, '削除'));
  };

  const renderOverview = () => {
    const s = stream;
    const live = s.live;
    replace(overview,
      kv([
        ['ID', h('code', null, s.id)],
        ['状態', [stateBadge(s), ' ', h('span', { class: 'muted small' }, `desired: ${s.desired_state} / observed: ${s.observed_state}`)]],
        ['遅延・コーデック', `${s.latency_profile} / ${s.codec_profile}`],
        ['HLS', `${s.hls.container}・${s.hls.segment_ms} ms × ${s.hls.window} 本`],
        ['録画', s.recording.enabled ? `自動（保持 ${s.recording.retention_days ? `${s.recording.retention_days} 日` : '無期限'}）` : '手動'],
        ['静止画', s.snapshots ? `${s.snapshots.interval_secs} 秒ごと・${s.snapshots.keep} 枚` : 'なし'],
        ['上限', `視聴 ${s.limits.max_viewers ?? '—'} / 受信 ${s.limits.max_ingress_bps ? fmtBps(s.limits.max_ingress_bps) : '—'}`],
        ['作成・更新', `${fmtTime(s.created_at)} / ${fmtTime(s.updated_at)}（世代 ${s.generation}）`],
        Object.keys(s.metadata).length ? ['メタデータ', Object.entries(s.metadata).map(([k, v]) => h('div', null, h('code', null, k), ' = ', v))] : null,
      ]),
      live ? h('div', null,
        h('h3', null, '受信中の入力'),
        kv([
          ['経路', live.transport],
          ['開始', `${fmtTime(live.started_at)}（${fmtRelative(live.started_at)}）`],
          ['epoch', String(live.epoch)],
          ['ビットレート', fmtBps(live.bitrate_bps)],
          ['視聴（目安）', String(live.viewers_hint)],
        ]),
        h('h3', null, 'トラック'),
        table([
          { label: '#', render: (t) => String(t.track ?? '') },
          { label: 'コーデック', render: (t) => h('code', null, t.codec || '?') },
          { label: '詳細', render: (t) => Object.entries(t).filter(([k]) => !['track', 'codec'].includes(k)).map(([k, v]) => `${k}=${v}`).join(' ') },
        ], live.tracks, { empty: 'トラックの情報はまだありません' }))
        : h('p', { class: 'muted' }, '今は入力がありません。'));
  };

  const renderIngest = () => {
    const ig = stream.ingest;
    const key = secret;
    replace(ingest,
      h('div', { class: 'form' },
        h('div', { class: 'field' }, h('strong', null, 'RTMP の URL'), copyField(ig.rtmp_url, 'RTMP の URL'),
          h('div', { class: 'hint' }, 'OBS などでは「サーバー」にこの URL、「ストリームキー」に下の鍵を入れます。')),
        ig.srt_url ? h('div', { class: 'field' }, h('strong', null, 'SRT の URL'), copyField(ig.srt_url, 'SRT の URL'),
          h('div', { class: 'hint' }, 'streamid は ', h('code', null, ig.srt_streamid_format || 'publish:{stream_key}'))) : null,
        ig.whip_url ? h('div', { class: 'field' }, h('strong', null, 'WHIP の URL'), copyField(ig.whip_url, 'WHIP の URL')) : null,
        h('div', { class: 'field' }, h('strong', null, 'ストリームキー'),
          key
            ? [h('div', { class: 'alert warn', role: 'alert' }, 'この鍵はもう表示されません。この画面を離れる前に控えてください。'),
              copyField(key, 'ストリームキー', { secret: true }),
              ig.rtmp_url ? h('div', { class: 'hint' }, '送り先の全体: ', copyField(`${ig.rtmp_url}/${key}`, 'RTMP の送り先（鍵入り）', { secret: true })) : null]
            : h('div', { class: 'muted' }, '鍵は作成と作り直しのときだけ表示されます（保存されていません）。'),
          h('div', null, h('button', { class: 'small', onClick: () => rotate() }, '鍵を作り直す')))));
  };

  const renderAll = () => {
    renderHead();
    renderOverview();
    renderIngest();
  };

  const reload = async () => {
    try {
      const before = stream ? JSON.stringify(stream) : '';
      const ingestBefore = stream ? JSON.stringify(stream.ingest) : '';
      await fetchStream();
      if (JSON.stringify(stream) === before) return;
      renderHead();
      renderOverview();
      // 取り込み先は変わったときだけ描き直す（写している途中のフォーカスを奪わない）
      if (JSON.stringify(stream.ingest) !== ingestBefore) renderIngest();
    } catch (e) {
      if (e.status === 404) {
        toast('この配信は削除されました', 'warn');
        navigate('/streams');
      } else reportError(e, '読み込み');
    }
  };

  const on412 = async (e) => {
    if (e.status === 412) {
      await reload();
      toast('他の操作で更新されていたので、最新の内容を読み込み直しました。もう一度操作してください', 'warn', 8000);
      return true;
    }
    return false;
  };

  const setDesired = async (value) => {
    try {
      const r = await streams.update(id, { desired_state: value }, etag);
      stream = r.data;
      etag = r.etag;
      renderAll();
      toast(value === 'stopped' ? '停止しました（publish を拒否します）' : '再開しました', 'ok');
    } catch (e) {
      if (!(await on412(e))) reportError(e, '状態の変更');
    }
  };

  const edit = async () => {
    const form = streamForm(stream, caps);
    const errBox = h('div');
    const updated = await modal(`「${stream.name}」を編集`, () => [form.el, errBox], {
      wide: true,
      actions: (close) => {
        const ok = h('button', { class: 'primary', onClick: async () => {
          const body = form.value();
          if (!body) return;
          if (!Object.keys(body).length) { close(null); return; }
          ok.disabled = true;
          try {
            const r = await streams.update(id, body, etag);
            etag = r.etag;
            close(r.data);
          } catch (e) {
            if (e.status === 412) {
              await reload();
              close(null);
              toast('他の操作で更新されていました。最新の内容を読み込み直したので、もう一度編集してください', 'warn', 9000);
              return;
            }
            replace(errBox, errorBox(e, { title: '保存できませんでした' }));
          } finally {
            ok.disabled = false;
          }
        } }, '保存');
        return [h('button', { onClick: () => close(null) }, 'やめる'), ok];
      },
    });
    if (updated) {
      stream = updated;
      renderAll();
      toast('保存しました', 'ok');
    }
  };

  const remove = async () => {
    const ok = await confirmDialog('配信を削除', `「${stream.name}」とその予約を削除します。publish 中なら切断します。録画は残ります。`, { ok: '削除', requireText: stream.name });
    if (!ok) return;
    try {
      await streams.remove(id, etag);
      toast('削除しました', 'ok');
      navigate('/streams');
    } catch (e) {
      if (!(await on412(e))) reportError(e, '削除');
    }
  };

  const rotate = async () => {
    const ok = await confirmDialog('鍵を作り直す', '新しい鍵を作ります。今の鍵はすぐに使えなくなり、送出中の送り手は再接続できなくなります。', { ok: '作り直す' });
    if (!ok) return;
    try {
      const r = await streams.rotateKey(id);
      stream = r.data;
      if (r.etag) etag = r.etag;
      secret = r.data.ingest.stream_key;
      renderAll();
      toast('鍵を作り直しました。表示されている鍵を控えてください', 'ok');
    } catch (e) {
      reportError(e, '鍵の作り直し');
    }
  };

  const startRecording = async () => {
    try {
      const { data } = await streams.startRecording(id);
      toast('録画を開始しました', 'ok');
      navigate(`/recordings/${encodeURIComponent(data.id)}`);
    } catch (e) {
      reportError(e, '録画の開始');
    }
  };

  // 今の画
  const auto = h('input', { type: 'checkbox' });
  let autoTimer = null;
  let lastSnap = null;
  const snapImg = h('img', { class: 'snapshot', alt: '今の画（静止画）', hidden: true });
  const snapMsg = h('div', { role: 'status', class: 'snap-msg' });
  const grab = async () => {
    try {
      const { data } = await streams.snapshot(id);
      const u = blobUrl(data);
      if (lastSnap) { URL.revokeObjectURL(lastSnap); blobUrls.delete(lastSnap); }
      lastSnap = u;
      snapImg.src = u;
      snapImg.hidden = false;
      replace(snapMsg, h('span', { class: 'muted small' }, `${new Date().toLocaleTimeString('ja-JP')} に取得`));
    } catch (e) {
      if (e.status === 401) return;
      let text = describeError(e);
      if (e.status === 409) text = `今の画はありません（${e.title}）。配信中でキーフレームを受け取った後に取れます。`;
      else if (e.status === 503) text = `今の画を作れません: ${e.title}。${e.retryable ? 'しばらく待って再試行してください。' : '映像を復号できる装置（GPU・ソフトウェア復号器）がこのノードにありません。H.264/H.265 はハードウェアが要ります。'}`;
      else if (e.status === 422) text = `このコーデックは方針により復号できません（${e.title}）`;
      replace(snapMsg, h('div', { class: 'alert warn' }, text, e.requestId ? h('div', { class: 'meta' }, `${e.code || e.status} · request_id: ${e.requestId}`) : null));
    }
  };
  auto.addEventListener('change', () => {
    clearInterval(autoTimer);
    if (auto.checked) { grab(); autoTimer = setInterval(grab, 5000); }
  });
  replace(snapNow,
    h('div', { class: 'row' }, h('button', { onClick: grab }, '今の画を取る'), h('label', { class: 'check' }, auto, '5 秒ごとに更新')),
    h('div', { style: { marginTop: '8px' } }, snapImg),
    snapMsg);

  const loadGallery = () => load(gallery, async () => {
    const { data } = await streams.snapshots(id);
    const items = data.items.slice(-24).reverse();
    if (!items.length) return h('div', { class: 'empty' }, '保存した静止画はありません（配信の設定で「静止画」を有効にすると一定間隔で保存します）');
    const grid = h('div', { class: 'gallery' });
    for (const it of items) {
      const img = h('img', { alt: `${fmtTime(it.taken_at)} の静止画`, loading: 'lazy' });
      const fig = h('figure', null, img,
        h('figcaption', null,
          h('span', { title: `${it.width}×${it.height}・${fmtBytes(it.bytes)}・${it.reason}` }, new Date(it.taken_at).toLocaleTimeString('ja-JP')),
          h('button', { class: 'small', 'aria-label': `${fmtTime(it.taken_at)} の静止画を削除`, onClick: async () => {
            if (!(await confirmDialog('静止画を削除', `${fmtTime(it.taken_at)} の静止画を削除します。`, { ok: '削除' }))) return;
            try { await streams.deleteSnapshot(id, it.id); fig.remove(); } catch (e) { reportError(e, '削除'); }
          } }, '削除')));
      grid.appendChild(fig);
      streams.snapshotImage(id, it.id).then(({ data: b }) => { img.src = blobUrl(b); }).catch(() => { img.alt = '読み込めません'; });
    }
    return h('div', null, h('p', { class: 'small muted' }, `${data.items.length} 枚（新しい 24 枚を表示）`), grid);
  });

  const loadPlan = () => load(planBox, async () => {
    const { data } = await streams.plan(id);
    const modeKind = (m) => ({ passthrough: 'ok', remux: 'ok', repacketize: 'ok', transcode: 'warn', unsupported: 'danger' }[m] || '');
    return h('div', null,
      h('p', null, '状態: ', badge(data.state === 'ready' ? 'ready（入力が分かっている）' : 'provisional（publish 前の見込み）', data.state === 'ready' ? 'ok' : 'warn'),
        data.reason ? h('span', { class: 'muted small' }, ` ${data.reason}`) : null),
      table([
        { label: '出力', render: (o) => o.output },
        { label: 'トラック', render: (o) => o.track },
        { label: '扱い', render: (o) => badge(o.mode, modeKind(o.mode)) },
        { label: 'コンテナ・変換先', render: (o) => [o.container, o.to].filter(Boolean).join(' → ') || '—' },
        { label: '追加の遅延', class: 'num', render: (o) => (o.added_latency_ms !== undefined ? `${o.added_latency_ms} ms` : '—') },
        { label: '理由', render: (o) => (o.code ? [h('code', null, o.code), ' ', o.reason || ''] : o.reason || '') },
      ], data.outputs, { empty: '出力はありません' }));
  });

  const loadRecs = () => load(recBox, async () => {
    const { data } = await recordings.list({ stream: id, limit: 1000 });
    const items = data.items.sort((a, b) => b.started_at.localeCompare(a.started_at)).slice(0, 10);
    return table([
      { label: '開始', render: (r) => h('a', { href: `#/recordings/${encodeURIComponent(r.id)}` }, fmtTime(r.started_at)) },
      { label: '状態', render: recBadge },
      { label: '長さ', class: 'num', render: (r) => fmtDuration(r.duration_ms) },
      { label: '大きさ', class: 'num', render: (r) => fmtBytes(r.bytes) },
    ], items, { empty: 'この配信の録画はありません' });
  });

  try {
    await fetchStream();
  } catch (e) {
    root.append(h('h1', null, '配信'), errorBox(e), h('p', null, h('a', { href: '#/streams' }, '一覧へ戻る')));
    return null;
  }

  const container = stream.hls.container;
  root.append(
    h('nav', { class: 'small', 'aria-label': 'パンくず' }, h('a', { href: '#/streams' }, '配信'), ' / ', stream.name),
    head,
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 's-ov' }, h('h2', { id: 's-ov' }, '概要'), overview),
      h('section', { class: 'panel', 'aria-labelledby': 's-in' }, h('h2', { id: 's-in' }, '取り込み（送り先）'), ingest)),
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 's-pv' }, h('h2', { id: 's-pv' }, 'ライブのプレビュー'),
        container === 'ts'
          ? h('div', { class: 'alert info small' }, 'この配信の HLS は MPEG-TS です。Safari 以外ではブラウザで再生できないので、URL を VLC などで開いてください。')
          : null,
        player.el),
      h('section', { class: 'panel', 'aria-labelledby': 's-sn' }, h('h2', { id: 's-sn' }, '今の画'), snapNow)),
    h('section', { class: 'panel', 'aria-labelledby': 's-pl' }, h('h2', { id: 's-pl' }, '変換・資源の計画'),
      h('div', { class: 'toolbar' }, h('button', { onClick: loadPlan }, '計画を取る')), planBox),
    h('section', { class: 'panel', 'aria-labelledby': 's-ga' }, h('h2', { id: 's-ga' }, '保存した静止画',
      h('button', { class: 'small', onClick: loadGallery }, '再読み込み')), gallery),
    h('section', { class: 'panel', 'aria-labelledby': 's-rc' }, h('h2', { id: 's-rc' }, '最近の録画',
      h('a', { class: 'small', href: '#/recordings' }, 'すべて')), recBox));
  renderAll();
  loadGallery();
  loadRecs();

  const offs = [
    refreshOn(['stream.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id }),
    refreshOn(['recording.'], () => { loadRecs(); reload(); }),
  ];
  // publish の開始・終了とビットレートは SSE のイベントにならないので、見えている間は 5 秒ごとに読む
  timers.push(setInterval(() => {
    if (document.visibilityState === 'visible') reload();
  }, 5000));
  return () => {
    offs.forEach((off) => off());
    player.stop();
    clearInterval(autoTimer);
    timers.forEach(clearInterval);
    for (const u of blobUrls) URL.revokeObjectURL(u);
    secret = null;
  };
}
