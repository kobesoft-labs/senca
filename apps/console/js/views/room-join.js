// 「このブラウザで参加して試す」: カメラ・マイクを確かめてから、ブラウザー SDK（apps/sdk-js、js/vendor/senca-sdk）で部屋に入る。
//
// - トークンはこの画面のログインの権限で発行する（client_id は console-xxxx、名前は入力したもの、1 時間）。保存しない。
// - SDK は参加するときに初めて読み込む（import()）。部屋の映像のコーデック・SDP・再接続は SDK がする。
// - 受けた映像は参加者ごとのタイルに出し、サイマルキャストなら層を選べる。統計は reportStats でノードへ送る（参加者の表に出る）。
// - 画面を離れたら（片付け）退出して、カメラ・マイクを止める。

import { h, replace, select } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { button, iconButton, formField, alert, badge } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rooms } from '../api/realtime.js';
import { participantName, signalingUrl } from '../lib/realtime.js';

const rand = () => Math.random().toString(36).slice(2, 8);

function mediaError(e) {
  if (!e) return t('rooms.join.errUnknown');
  if (e.name === 'NotAllowedError') return t('rooms.join.errDenied');
  if (e.name === 'NotFoundError' || e.name === 'OverconstrainedError') return t('rooms.join.errNoDevice');
  if (e.name === 'NotReadableError') return t('rooms.join.errBusy');
  if (e.code) return t('rooms.join.errSdk', { code: e.code, message: e.message || '' });
  return e.message || String(e);
}

/**
 * @param {() => object} getRoom 今の部屋
 * @param {{onChange?: () => void}} [o] 参加・退出したとき（参加者の表を読み直す）
 */
export function joinPanel(getRoom, { onChange } = {}) {
  let media = null;
  let session = null;
  let abort = null;
  const tiles = new Map(); // participantId → {el, video, audio, layerSel}

  const name = h('input', { type: 'text', value: t('rooms.join.defaultName'), maxlength: 60, 'data-testid': 'join-name' });
  const useMedia = h('input', { type: 'checkbox', checked: true, 'data-testid': 'join-use-media' });
  const simulcast = h('input', { type: 'checkbox', checked: true, 'data-testid': 'join-simulcast' });
  const localVideo = h('video', { muted: true, playsinline: true, autoplay: true, 'aria-label': t('rooms.join.localLabel') });
  const localTile = h('figure', { class: 'join-tile local', 'data-testid': 'join-local' }, localVideo,
    h('div', { class: 'join-placeholder' }, icon('camera'), h('span', null, t('rooms.join.noPreview'))),
    h('figcaption', null, h('span', { class: 'join-name' }, t('rooms.join.you')), h('span', { class: 'join-flags' })));
  const stage = h('div', { class: 'join-stage', 'data-testid': 'join-stage' }, localTile);
  const statusEl = h('div', { class: 'join-status', role: 'status', 'aria-live': 'polite', 'data-state': 'idle', 'data-testid': 'join-status' }, t('rooms.join.idle'));
  const errBox = h('div');
  const options = h('div', { class: 'join-options' },
    formField({ label: t('rooms.join.name'), input: name, hint: t('rooms.join.nameHint') }),
    h('div', { class: 'stack' },
      h('label', { class: 'check' }, useMedia, t('rooms.join.useMedia')),
      h('label', { class: 'check' }, simulcast, t('rooms.join.simulcast')),
      h('div', { class: 'hint' }, t('rooms.join.simulcastHint'))));

  const setStatus = (state, text) => { statusEl.dataset.state = state; statusEl.textContent = text; };
  const syncLocal = () => {
    localTile.classList.toggle('has-video', !!(media && media.getVideoTracks().some((x) => x.readyState === 'live')));
    const flags = localTile.querySelector('.join-flags');
    const a = media && media.getAudioTracks()[0];
    const v = media && media.getVideoTracks()[0];
    replace(flags,
      a && !a.enabled ? icon('mic-off', { size: 'sm', label: t('rooms.join.micOff') }) : null,
      v && !v.enabled ? icon('video-off', { size: 'sm', label: t('rooms.join.camOff') }) : null);
  };

  const previewBtn = button(t('rooms.join.preview'), { icon: 'camera', testid: 'join-preview', onClick: () => preview() });
  const joinBtn = button(t('rooms.join.join'), { icon: 'login', testid: 'join-start', onClick: () => join() });
  const leaveBtn = button(t('rooms.join.leave'), { icon: 'phone-off', testid: 'join-leave', attrs: { hidden: true }, onClick: () => leave() });
  const micBtn = iconButton('mic', t('rooms.join.mic'), { variant: 'secondary', testid: 'join-mic', attrs: { 'aria-pressed': 'true', hidden: true } });
  const camBtn = iconButton('send', t('rooms.join.cam'), { variant: 'secondary', testid: 'join-cam', attrs: { 'aria-pressed': 'true', hidden: true } });
  const toggleTrack = (btn, kind, onIcon, offIcon) => {
    btn.addEventListener('click', () => {
      const tr = media && (kind === 'audio' ? media.getAudioTracks()[0] : media.getVideoTracks()[0]);
      if (!tr) return;
      tr.enabled = !tr.enabled;
      btn.setAttribute('aria-pressed', String(tr.enabled));
      btn.replaceChildren(icon(tr.enabled ? onIcon : offIcon));
      syncLocal();
    });
  };
  toggleTrack(micBtn, 'audio', 'mic', 'mic-off');
  toggleTrack(camBtn, 'video', 'send', 'video-off');

  const stopMedia = () => {
    if (media) media.getTracks().forEach((x) => x.stop());
    media = null;
    localVideo.srcObject = null;
    micBtn.hidden = true;
    camBtn.hidden = true;
    syncLocal();
  };

  const preview = async () => {
    replace(errBox);
    try {
      if (!media) {
        media = await navigator.mediaDevices.getUserMedia({ video: { width: { ideal: 1280 }, height: { ideal: 720 } }, audio: true });
        localVideo.srcObject = media;
      }
      micBtn.hidden = false;
      camBtn.hidden = false;
      syncLocal();
      setStatus('ready', t('rooms.join.previewing'));
    } catch (e) {
      stopMedia();
      replace(errBox, alert('warn', mediaError(e), { small: true }));
    }
  };

  const removeTile = (pid) => {
    const x = tiles.get(pid);
    if (x) { x.el.remove(); tiles.delete(pid); }
  };
  const tileFor = (track) => {
    const pid = track.participantId || track.id;
    let x = tiles.get(pid);
    if (!x) {
      const label = track.participant ? participantName({ ...track.participant, connection_id: track.participantId, client_id: track.clientId }) : (track.clientId || pid.slice(0, 8));
      const video = h('video', { playsinline: true, autoplay: true, muted: true, 'aria-label': t('rooms.join.remoteLabel', { who: label }) });
      const audio = h('audio', { autoplay: true });
      const layerSel = select([['auto', t('rooms.join.layerAuto')]], 'auto', { 'aria-label': t('rooms.join.layer', { who: label }), class: 'join-layer', hidden: true });
      const el = h('figure', { class: 'join-tile remote', 'data-testid': 'join-remote' }, video, audio,
        h('div', { class: 'join-placeholder' }, icon('user'), h('span', null, label)),
        h('figcaption', null, h('span', { class: 'join-name' }, label), layerSel));
      x = { el, video, audio, layerSel };
      tiles.set(pid, x);
      stage.append(el);
    }
    return x;
  };
  const onTrack = (track) => {
    const x = tileFor(track);
    if (track.kind === 'video') {
      track.attach(x.video);
      x.el.classList.add('has-video');
      if (track.rids && track.rids.length > 1) {
        replace(x.layerSel, [['auto', t('rooms.join.layerAuto')], ...track.rids.map((r) => [r, r])].map(([v, l]) => h('option', { value: v }, l)));
        x.layerSel.hidden = false;
        x.layerSel.onchange = () => track.setLayer(x.layerSel.value).catch(() => {});
      }
    } else {
      track.attach(x.audio);
    }
    track.ended.then(() => {
      if (track.kind === 'video') x.el.classList.remove('has-video');
      if (![...(session ? session.tracks.values() : [])].some((o) => (o.participantId || o.id) === (track.participantId || track.id) && o.state !== 'ended')) removeTile(track.participantId || track.id);
    });
  };

  const setJoined = (on) => {
    joinBtn.hidden = on;
    previewBtn.hidden = on;
    leaveBtn.hidden = !on;
    options.hidden = on;
  };

  const join = async () => {
    const room = getRoom();
    replace(errBox);
    if (room.settings.e2ee) { replace(errBox, alert('info', t('rooms.join.e2ee'), { small: true })); return; }
    if (useMedia.checked && !media) {
      await preview();
      if (!media) return;
    }
    setStatus('connecting', t('rooms.join.connecting'));
    abort = new AbortController();
    const clientId = `console-${rand()}`;
    const role = useMedia.checked ? 'sendrecv' : 'recvonly';
    try {
      const { connect } = await import('../vendor/senca-sdk/realtime/session.js');
      let url = room.signaling_url;
      session = await connect({
        url: signalingUrl(url),
        room: room.id,
        role,
        token: async () => {
          const { data } = await rooms.token(room.id, { role, client_id: clientId, metadata: { name: name.value.trim() || clientId, via: 'console' }, ttl_secs: 3600 });
          url = data.signaling_url || url;
          return data.token;
        },
        publish: useMedia.checked ? { stream: media, simulcast: simulcast.checked } : undefined,
        reportStats: { intervalMs: 2000 },
        signal: abort.signal,
      });
      setJoined(true);
      setStatus('joined', t('rooms.join.joined', { id: session.connectionId }));
      if (onChange) onChange();
      const signal = abort.signal;
      (async () => { for await (const tr of session.remoteTracks({ signal })) onTrack(tr); })().catch(() => {});
      (async () => {
        for await (const ev of session.events({ signal, types: ['participant.left', 'reconnecting', 'reconnected'] })) {
          if (ev.type === 'participant.left') removeTile(ev.participant.id);
          else if (ev.type === 'reconnecting') setStatus('connecting', t('rooms.join.reconnecting', { n: ev.attempt }));
          else if (ev.type === 'reconnected') setStatus('joined', t('rooms.join.joined', { id: ev.connectionId }));
        }
      })().catch(() => {});
      session.closed.then(({ reason, error }) => {
        if (!session) return;
        session = null;
        cleanupRemote();
        setJoined(false);
        if (reason === 'normal' || reason === 'aborted') setStatus('idle', t('rooms.join.left'));
        else {
          setStatus('error', t('rooms.join.closed', { reason }));
          if (error) replace(errBox, alert('warn', mediaError(error), { small: true }));
        }
        if (onChange) onChange();
      });
    } catch (e) {
      session = null;
      if (e && e.name === 'AbortError') { setStatus('idle', t('rooms.join.left')); return; }
      setStatus('error', t('rooms.join.failed'));
      replace(errBox, alert('warn', mediaError(e), { small: true, testid: 'join-error' }));
    }
  };

  const cleanupRemote = () => { for (const pid of [...tiles.keys()]) removeTile(pid); };

  const leave = async () => {
    const s = session;
    session = null;
    if (abort) abort.abort();
    abort = null;
    if (s) await s.close().catch(() => {});
    cleanupRemote();
    setJoined(false);
    setStatus('idle', t('rooms.join.left'));
    if (onChange) onChange();
  };

  const el = h('div', { class: 'join' },
    h('p', { class: 'small muted' }, t('rooms.join.lead')),
    stage,
    h('div', { class: 'join-controls' }, previewBtn, joinBtn, micBtn, camBtn, leaveBtn, h('span', { class: 'spacer' }), statusEl),
    errBox,
    options);

  // E2EE のルームでは、このブラウザは暗号化できないので試せない（押してから断るのではなく、先に伝える）
  const view = getRoom().settings.e2ee ? alert('info', t('rooms.join.e2ee'), { testid: 'join-e2ee' }) : el;
  return {
    el: view,
    /** このブラウザの connection_id（参加していなければ null）。 */
    connectionId: () => (session ? session.connectionId : null),
    badge: () => badge(t('rooms.join.thisBrowser'), 'accent', { icon: 'monitor' }),
    stop: () => {
      if (abort) abort.abort();
      if (session) session.close().catch(() => {});
      session = null;
      stopMedia();
    },
  };
}
