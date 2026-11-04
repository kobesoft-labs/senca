// @senca/sdk: SENCA のリアルタイムの部屋（ブラウザー）と管理 API（ブラウザー・Node.js）の SDK。
//
//   import { Senca, SencaClient } from '@senca/sdk';
//   const session = await Senca.connect({ url: 'wss://node.example/signaling', room: 'daily', token });

import { connect, Session, SDK_NAME, SDK_VERSION } from './realtime/session.js';

export { connect, Session, SDK_NAME, SDK_VERSION };
export { Participant, RemoteTrack, Publication, LocalTrack, InboundMessage } from './realtime/objects.js';
export { DEFAULT_SIMULCAST } from './realtime/media.js';
export { SUBPROTOCOL, PROTOCOL_VERSION } from './realtime/protocol.js';
export { SencaError, ApiError } from './errors.js';
export { SencaClient, Collection, Streams, Recordings, Schedules, Tokens, StreamIo, Secrets, Pipelines, Nodes, Rooms, MediaPipelines, Audit, Auth, Users } from './rest/client.js';
export { Paginator, etagOf } from './rest/http.js';
export { EventStreamParser } from './rest/sse.js';

/** 入口（`Senca.connect()`）。 */
export const Senca = Object.freeze({
  /**
   * 部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。
   * @param {import('./realtime/session.js').ConnectOptions} options
   * @returns {Promise<Session>}
   */
  connect: (options) => connect(options),
  version: SDK_VERSION,
});

// ------------------------------------------------------------------ 型（JSDoc の typedef を型定義で公開する）

/** @typedef {import('./realtime/session.js').ConnectOptions} ConnectOptions */
/** @typedef {import('./realtime/session.js').ReconnectOptions} ReconnectOptions */
/** @typedef {import('./realtime/session.js').DataChannelOption} DataChannelOption */
/** @typedef {import('./realtime/session.js').SessionEvent} SessionEvent */
/** @typedef {import('./realtime/session.js').SessionEventType} SessionEventType */
/** @typedef {import('./realtime/session.js').RoomInfo} RoomInfo */
/** @typedef {import('./realtime/session.js').NetworkStatus} NetworkStatus */
/** @typedef {import('./realtime/session.js').CloseInfo} CloseInfo */
/** @typedef {import('./realtime/session.js').Environment} Environment */
/** @typedef {import('./realtime/objects.js').PublishOptions} PublishOptions */
/** @typedef {import('./realtime/objects.js').PublishedTrackInfo} PublishedTrackInfo */
/** @typedef {import('./realtime/objects.js').RemoteTrackEvent} RemoteTrackEvent */
/** @typedef {import('./realtime/media.js').SimulcastLayer} SimulcastLayer */
/** @typedef {import('./realtime/media.js').SessionStats} SessionStats */
/** @typedef {import('./realtime/media.js').InboundStats} InboundStats */
/** @typedef {import('./realtime/media.js').OutboundStats} OutboundStats */
/** @typedef {import('./realtime/protocol.js').Role} Role */
/** @typedef {import('./realtime/protocol.js').Kind} Kind */
/** @typedef {import('./realtime/protocol.js').VideoCodec} VideoCodec */
/** @typedef {import('./realtime/protocol.js').ForwardingFilter} ForwardingFilter */
/** @typedef {import('./errors.js').SencaErrorCode} SencaErrorCode */
/** @typedef {import('./rest/http.js').ClientOptions} ClientOptions */
/** @typedef {import('./rest/http.js').RetryOptions} RetryOptions */
/** @typedef {import('./rest/http.js').RequestOptions} RequestOptions */
/** @typedef {import('./rest/http.js').IfMatch} IfMatch */
/** @typedef {import('./rest/sse.js').ServerEvent} ServerEvent */
/** @typedef {import('./rest/sse.js').EventsOptions} EventsOptions */
/** @typedef {import('./rest/client.js').ListOptions} ListOptions */
/** @typedef {import('./rest/client.js').CreateOptions} CreateOptions */
/** @typedef {import('./rest/client.js').CallOptions} CallOptions */
/** @typedef {import('./rest/openapi.gen.js').Stream} Stream */
/** @typedef {import('./rest/openapi.gen.js').StreamCreate} StreamCreate */
/** @typedef {import('./rest/openapi.gen.js').StreamPatch} StreamPatch */
/** @typedef {import('./rest/openapi.gen.js').Recording} Recording */
/** @typedef {import('./rest/openapi.gen.js').Schedule} Schedule */
/** @typedef {import('./rest/openapi.gen.js').Token} Token */
/** @typedef {import('./rest/openapi.gen.js').Input} Input */
/** @typedef {import('./rest/openapi.gen.js').Output} Output */
/** @typedef {import('./rest/openapi.gen.js').Secret} Secret */
/** @typedef {import('./rest/openapi.gen.js').Pipeline} Pipeline */
/** @typedef {import('./rest/openapi.gen.js').Node} Node */
/** @typedef {import('./rest/openapi.gen.js').Grid} Grid */
/** @typedef {import('./rest/openapi.gen.js').Problem} Problem */
/** @typedef {import('./rest/openapi.gen.js').Room} Room */
/** @typedef {import('./rest/openapi.gen.js').Participant} RoomParticipant */
/** @typedef {import('./rest/openapi.gen.js').RoomToken} RoomToken */
