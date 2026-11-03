// シグナリング v1 の型（docs/realtime/signaling.md、crates/senca-rooms/src/signaling.rs が正）。
// SDK の利用者はふつう触らない（Session が隠す）。試験と、プロトコルを直に扱いたい人のために公開する。

/** WebSocket の subprotocol。 */
export const SUBPROTOCOL = 'senca.rooms.v1';
/** プロトコルの版。 */
export const PROTOCOL_VERSION = 1;
/** アプリの message の本文の上限（byte）。 */
export const MAX_APP_MESSAGE_BYTES = 64 * 1024;
/** シグナリングを載せ替える DataChannel のラベル。 */
export const SIGNALING_LABEL = 'signaling';

/** @typedef {'sendrecv' | 'sendonly' | 'recvonly'} Role */
/** @typedef {'audio' | 'video'} Kind */
/** @typedef {'vp8' | 'vp9' | 'h264' | 'av1'} VideoCodec */
/** @typedef {'opus'} AudioCodec */
/** @typedef {'lowest' | 'off'} Unfocused */
/** @typedef {'sendrecv' | 'sendonly' | 'recvonly'} DataDirection */

/**
 * @typedef {object} WireDataChannel
 * @property {string} label
 * @property {DataDirection} [direction]
 * @property {boolean} [ordered]
 * @property {number} [max_retransmits]
 * @property {number} [max_packet_life_time]
 */

/**
 * 転送フィルター（§8）。規則は送り手の属性を見る。`rules` は AND の組の OR。
 * @typedef {object} ForwardingFilter
 * @property {'block' | 'allow'} action
 * @property {Array<Array<{field: string, operator: 'is_in' | 'is_not_in', values: string[]}>>} rules
 */

/**
 * @typedef {object} ConnectMessage
 * @property {'connect'} type
 * @property {1} version
 * @property {string} room_id
 * @property {string} [token]
 * @property {Role} role
 * @property {string} [client_id]
 * @property {string} [bundle_id]
 * @property {unknown} [metadata]
 * @property {boolean} [multistream]
 * @property {{rids: string[], default_rid?: string}} [simulcast]
 * @property {{number: number, focus_rid?: string, unfocused?: Unfocused}} [spotlight]
 * @property {{enabled?: boolean, codec?: AudioCodec, bitrate_kbps?: number}} [audio]
 * @property {{enabled?: boolean, codec?: VideoCodec, bitrate_kbps?: number}} [video]
 * @property {WireDataChannel[]} [data_channels]
 * @property {{data_channel?: boolean, close_websocket?: boolean}} [signaling]
 * @property {boolean} [e2ee]
 * @property {ForwardingFilter} [forwarding_filter]
 * @property {{name: string, version: string}} [sdk]
 */

/**
 * @typedef {ConnectMessage
 *   | {type: 'offer', sdp: string}
 *   | {type: 'candidate', candidate: string, sdp_mid?: string, sdp_mline_index?: number, username_fragment?: string}
 *   | {type: 'pong', stats?: unknown}
 *   | {type: 'layer', track_id: string, rid: string | null}
 *   | {type: 'message', label: string, to?: string[], data?: string, data_base64?: string}
 *   | {type: 'stats', reports: unknown}
 *   | {type: 'disconnect', reason?: string}} ClientMessage
 */

/**
 * @typedef {object} WireTrack
 * @property {string} track_id
 * @property {string} connection_id
 * @property {Kind} kind
 * @property {string[]} [rids]
 */

/**
 * @typedef {object} WireParticipant
 * @property {string} connection_id
 * @property {string} [client_id]
 * @property {string} bundle_id
 * @property {Role} role
 * @property {unknown} metadata
 * @property {WireTrack[]} tracks
 * @property {string} [node]
 */

/**
 * @typedef {object} WireSlot
 * @property {string} mid
 * @property {Kind} kind
 * @property {'empty' | 'active' | 'paused'} state
 * @property {string} [track_id]
 * @property {string} [connection_id]
 * @property {string} [client_id]
 */

/**
 * @typedef {object} ConnectedMessage
 * @property {'connected'} type
 * @property {number} version
 * @property {string} connection_id
 * @property {string} [client_id]
 * @property {string} bundle_id
 * @property {string} room_id
 * @property {Role} role
 * @property {boolean} multistream
 * @property {boolean} e2ee
 * @property {VideoCodec} video_codec
 * @property {AudioCodec} audio_codec
 * @property {number} [max_video_bitrate_kbps]
 * @property {{number: number, focus_rid?: string, unfocused?: Unfocused}} [spotlight]
 * @property {WireParticipant[]} participants
 * @property {{audio: number, video: number}} recv_slots
 * @property {WireDataChannel[]} data_channels
 * @property {boolean} signaling_data_channel
 * @property {number} ping_interval_ms
 * @property {unknown} metadata
 */

/**
 * @typedef {{event: 'connection.created', participant: WireParticipant}
 *   | {event: 'connection.updated', connection_id: string, metadata: unknown}
 *   | {event: 'connection.destroyed', connection_id: string, client_id?: string, reason: string}
 *   | {event: 'track.added', track: WireTrack}
 *   | {event: 'track.removed', track_id: string, connection_id: string}
 *   | {event: 'spotlight.changed', focused: string[]}
 *   | {event: 'layer.changed', track_id: string, mid: string, rid: string | null}
 *   | {event: 'network.status', level: number, bandwidth_bps: number, rtt_ms: number}} NotifyBody
 */

/**
 * @typedef {ConnectedMessage
 *   | {type: 'answer', sdp: string, slots: WireSlot[], published: Array<{mid: string, track_id: string, kind: Kind, rids?: string[]}>}
 *   | {type: 'slots', slots: WireSlot[]}
 *   | {type: 'renegotiate', audio: number, video: number}
 *   | ({type: 'notify'} & NotifyBody)
 *   | {type: 'push', data: unknown}
 *   | {type: 'message', label: string, from?: string, data?: string, data_base64?: string}
 *   | {type: 'ping'}
 *   | {type: 'switched'}
 *   | {type: 'redirect', location: string}
 *   | {type: 'error', code: string, message: string, fatal: boolean, request?: string}
 *   | {type: 'disconnect', reason: string}} ServerMessage
 */

/** 誤りの code → WebSocket の close code（§7）。 */
export const CLOSE_CODES = Object.freeze({
  bad_message: 4400, invalid_sdp: 4400, invalid_state: 4400,
  unsupported_version: 4422, unsupported_codec: 4422, e2ee_mismatch: 4422,
  auth_failed: 4401, forbidden: 4403, role_violation: 4403, unknown_label: 4403,
  room_not_found: 4404, unknown_track: 4404, room_full: 4503, rate_limited: 4429,
  timeout: 4408, negotiation_failed: 4500, internal: 4500,
});

/** close code → 誤りの code（close だけが届いたとき）。 */
export const CODE_BY_CLOSE = Object.freeze({
  4400: 'bad_message', 4401: 'auth_failed', 4403: 'forbidden', 4404: 'room_not_found', 4408: 'timeout',
  4422: 'unsupported_version', 4429: 'rate_limited', 4500: 'internal', 4503: 'room_full',
});
