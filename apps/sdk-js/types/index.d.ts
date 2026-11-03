export { DEFAULT_SIMULCAST } from "./realtime/media.js";
export { EventStreamParser } from "./rest/sse.js";
/** 入口（`Senca.connect()`）。 */
export const Senca: Readonly<{
    /**
     * 部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。
     * @param {import('./realtime/session.js').ConnectOptions} options
     * @returns {Promise<Session>}
     */
    connect: (options: import("./realtime/session.js").ConnectOptions) => Promise<Session>;
    version: "0.0.0";
}>;
export type ConnectOptions = import("./realtime/session.js").ConnectOptions;
export type ReconnectOptions = import("./realtime/session.js").ReconnectOptions;
export type DataChannelOption = import("./realtime/session.js").DataChannelOption;
export type SessionEvent = import("./realtime/session.js").SessionEvent;
export type SessionEventType = import("./realtime/session.js").SessionEventType;
export type RoomInfo = import("./realtime/session.js").RoomInfo;
export type NetworkStatus = import("./realtime/session.js").NetworkStatus;
export type CloseInfo = import("./realtime/session.js").CloseInfo;
export type Environment = import("./realtime/session.js").Environment;
export type PublishOptions = import("./realtime/objects.js").PublishOptions;
export type PublishedTrackInfo = import("./realtime/objects.js").PublishedTrackInfo;
export type RemoteTrackEvent = import("./realtime/objects.js").RemoteTrackEvent;
export type SimulcastLayer = import("./realtime/media.js").SimulcastLayer;
export type SessionStats = import("./realtime/media.js").SessionStats;
export type InboundStats = import("./realtime/media.js").InboundStats;
export type OutboundStats = import("./realtime/media.js").OutboundStats;
export type Role = import("./realtime/protocol.js").Role;
export type Kind = import("./realtime/protocol.js").Kind;
export type VideoCodec = import("./realtime/protocol.js").VideoCodec;
export type ForwardingFilter = import("./realtime/protocol.js").ForwardingFilter;
export type SencaErrorCode = import("./errors.js").SencaErrorCode;
export type ClientOptions = import("./rest/http.js").ClientOptions;
export type RetryOptions = import("./rest/http.js").RetryOptions;
export type RequestOptions = import("./rest/http.js").RequestOptions;
export type IfMatch = import("./rest/http.js").IfMatch;
export type ServerEvent = import("./rest/sse.js").ServerEvent;
export type EventsOptions = import("./rest/sse.js").EventsOptions;
export type ListOptions = import("./rest/client.js").ListOptions;
export type CreateOptions = import("./rest/client.js").CreateOptions;
export type CallOptions = import("./rest/client.js").CallOptions;
export type Stream = import("./rest/openapi.gen.js").Stream;
export type StreamCreate = import("./rest/openapi.gen.js").StreamCreate;
export type StreamPatch = import("./rest/openapi.gen.js").StreamPatch;
export type Recording = import("./rest/openapi.gen.js").Recording;
export type Schedule = import("./rest/openapi.gen.js").Schedule;
export type Token = import("./rest/openapi.gen.js").Token;
export type Input = import("./rest/openapi.gen.js").Input;
export type Output = import("./rest/openapi.gen.js").Output;
export type Secret = import("./rest/openapi.gen.js").Secret;
export type Pipeline = import("./rest/openapi.gen.js").Pipeline;
export type Node = import("./rest/openapi.gen.js").Node;
export type Grid = import("./rest/openapi.gen.js").Grid;
export type Problem = import("./rest/openapi.gen.js").Problem;
export type Room = import("./rest/openapi.gen.js").Room;
export type RoomParticipant = import("./rest/openapi.gen.js").Participant;
export type RoomToken = import("./rest/openapi.gen.js").RoomToken;
import { connect } from './realtime/session.js';
import { Session } from './realtime/session.js';
import { SDK_NAME } from './realtime/session.js';
import { SDK_VERSION } from './realtime/session.js';
export { connect, Session, SDK_NAME, SDK_VERSION };
export { Participant, RemoteTrack, Publication, LocalTrack, InboundMessage } from "./realtime/objects.js";
export { SUBPROTOCOL, PROTOCOL_VERSION } from "./realtime/protocol.js";
export { SencaError, ApiError } from "./errors.js";
export { SencaClient, Collection, Streams, Recordings, Schedules, Tokens, StreamIo, Secrets, Pipelines, Nodes, Rooms, MediaPipelines } from "./rest/client.js";
export { Paginator, etagOf } from "./rest/http.js";
