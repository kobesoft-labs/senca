/** WebSocket の subprotocol。 */
export const SUBPROTOCOL: "senca.rooms.v1";
/** プロトコルの版。 */
export const PROTOCOL_VERSION: 1;
/** アプリの message の本文の上限（byte）。 */
export const MAX_APP_MESSAGE_BYTES: number;
/** シグナリングを載せ替える DataChannel のラベル。 */
export const SIGNALING_LABEL: "signaling";
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
export const CLOSE_CODES: Readonly<{
    bad_message: 4400;
    invalid_sdp: 4400;
    invalid_state: 4400;
    unsupported_version: 4422;
    unsupported_codec: 4422;
    e2ee_mismatch: 4422;
    auth_failed: 4401;
    forbidden: 4403;
    role_violation: 4403;
    unknown_label: 4403;
    room_not_found: 4404;
    unknown_track: 4404;
    room_full: 4503;
    rate_limited: 4429;
    timeout: 4408;
    negotiation_failed: 4500;
    internal: 4500;
}>;
/** close code → 誤りの code（close だけが届いたとき）。 */
export const CODE_BY_CLOSE: Readonly<{
    4400: "bad_message";
    4401: "auth_failed";
    4403: "forbidden";
    4404: "room_not_found";
    4408: "timeout";
    4422: "unsupported_version";
    4429: "rate_limited";
    4500: "internal";
    4503: "room_full";
}>;
export type Role = "sendrecv" | "sendonly" | "recvonly";
export type Kind = "audio" | "video";
export type VideoCodec = "vp8" | "vp9" | "h264" | "av1";
export type AudioCodec = "opus";
export type Unfocused = "lowest" | "off";
export type DataDirection = "sendrecv" | "sendonly" | "recvonly";
export type WireDataChannel = {
    label: string;
    direction?: DataDirection | undefined;
    ordered?: boolean | undefined;
    max_retransmits?: number | undefined;
    max_packet_life_time?: number | undefined;
};
/**
 * 転送フィルター（§8）。規則は送り手の属性を見る。`rules` は AND の組の OR。
 */
export type ForwardingFilter = {
    action: "block" | "allow";
    rules: Array<Array<{
        field: string;
        operator: "is_in" | "is_not_in";
        values: string[];
    }>>;
};
export type ConnectMessage = {
    type: "connect";
    version: 1;
    room_id: string;
    token?: string | undefined;
    role: Role;
    client_id?: string | undefined;
    bundle_id?: string | undefined;
    metadata?: unknown;
    multistream?: boolean | undefined;
    simulcast?: {
        rids: string[];
        default_rid?: string;
    } | undefined;
    spotlight?: {
        number: number;
        focus_rid?: string;
        unfocused?: Unfocused;
    } | undefined;
    audio?: {
        enabled?: boolean;
        codec?: AudioCodec;
        bitrate_kbps?: number;
    } | undefined;
    video?: {
        enabled?: boolean;
        codec?: VideoCodec;
        bitrate_kbps?: number;
    } | undefined;
    data_channels?: WireDataChannel[] | undefined;
    signaling?: {
        data_channel?: boolean;
        close_websocket?: boolean;
    } | undefined;
    e2ee?: boolean | undefined;
    forwarding_filter?: ForwardingFilter | undefined;
    sdk?: {
        name: string;
        version: string;
    } | undefined;
};
export type ClientMessage = ConnectMessage | {
    type: "offer";
    sdp: string;
} | {
    type: "candidate";
    candidate: string;
    sdp_mid?: string;
    sdp_mline_index?: number;
    username_fragment?: string;
} | {
    type: "pong";
    stats?: unknown;
} | {
    type: "layer";
    track_id: string;
    rid: string | null;
} | {
    type: "message";
    label: string;
    to?: string[];
    data?: string;
    data_base64?: string;
} | {
    type: "stats";
    reports: unknown;
} | {
    type: "disconnect";
    reason?: string;
};
export type WireTrack = {
    track_id: string;
    connection_id: string;
    kind: Kind;
    rids?: string[] | undefined;
};
export type WireParticipant = {
    connection_id: string;
    client_id?: string | undefined;
    bundle_id: string;
    role: Role;
    metadata: unknown;
    tracks: WireTrack[];
    node?: string | undefined;
};
export type WireSlot = {
    mid: string;
    kind: Kind;
    state: "empty" | "active" | "paused";
    track_id?: string | undefined;
    connection_id?: string | undefined;
    client_id?: string | undefined;
};
export type ConnectedMessage = {
    type: "connected";
    version: number;
    connection_id: string;
    client_id?: string | undefined;
    bundle_id: string;
    room_id: string;
    role: Role;
    multistream: boolean;
    e2ee: boolean;
    video_codec: VideoCodec;
    audio_codec: AudioCodec;
    max_video_bitrate_kbps?: number | undefined;
    spotlight?: {
        number: number;
        focus_rid?: string;
        unfocused?: Unfocused;
    } | undefined;
    participants: WireParticipant[];
    recv_slots: {
        audio: number;
        video: number;
    };
    data_channels: WireDataChannel[];
    signaling_data_channel: boolean;
    ping_interval_ms: number;
    metadata: unknown;
};
export type NotifyBody = {
    event: "connection.created";
    participant: WireParticipant;
} | {
    event: "connection.updated";
    connection_id: string;
    metadata: unknown;
} | {
    event: "connection.destroyed";
    connection_id: string;
    client_id?: string;
    reason: string;
} | {
    event: "track.added";
    track: WireTrack;
} | {
    event: "track.removed";
    track_id: string;
    connection_id: string;
} | {
    event: "spotlight.changed";
    focused: string[];
} | {
    event: "layer.changed";
    track_id: string;
    mid: string;
    rid: string | null;
} | {
    event: "network.status";
    level: number;
    bandwidth_bps: number;
    rtt_ms: number;
};
export type ServerMessage = ConnectedMessage | {
    type: "answer";
    sdp: string;
    slots: WireSlot[];
    published: Array<{
        mid: string;
        track_id: string;
        kind: Kind;
        rids?: string[];
    }>;
} | {
    type: "slots";
    slots: WireSlot[];
} | {
    type: "renegotiate";
    audio: number;
    video: number;
} | ({
    type: "notify";
} & NotifyBody) | {
    type: "push";
    data: unknown;
} | {
    type: "message";
    label: string;
    from?: string;
    data?: string;
    data_base64?: string;
} | {
    type: "ping";
} | {
    type: "switched";
} | {
    type: "redirect";
    location: string;
} | {
    type: "error";
    code: string;
    message: string;
    fatal: boolean;
    request?: string;
} | {
    type: "disconnect";
    reason: string;
};
