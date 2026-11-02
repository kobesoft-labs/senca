//! ICE（RFC 8445）のエージェント、candidate 属性（RFC 8839）、trickle ICE（RFC 8838）、
//! consent freshness（RFC 7675）、packet の振り分け（RFC 7983）。sans-io。
//!
//! 駆動の仕方は [`agent`] を見ること。

pub mod agent;
pub mod candidate;
pub mod demux;

pub use agent::{
    Credentials, IceAgent, IceConfig, IceError, IceEvent, IceRole, IceState, MdnsResolver,
    TurnServerSpec,
};
pub use candidate::{Candidate, CandidateAddress, CandidateKind, CandidateParseError, Transport};
pub use demux::{PacketKind, classify};
pub use senca_turn::Transmit;
