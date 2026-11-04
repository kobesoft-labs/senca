//! openapi.yaml の operationId と、この SDK のメソッドの対応表。試験が仕様と突き合わせ、
//! 仕様に操作が足されたのに SDK に無い（またはその逆）ときに失敗する。

/// `(operationId, メソッド)`。
pub const OPERATIONS: &[(&str, &str)] = &[
    ("getCapabilities", "SencaClient::capabilities"),
    ("listNodes", "Nodes::list"),
    ("getNode", "Nodes::get"),
    ("drainNode", "Nodes::drain"),
    ("undrainNode", "Nodes::undrain"),
    ("listDevices", "SencaClient::devices"),
    ("getGrid", "SencaClient::grid"),
    ("disableDevice", "Nodes::disable_device"),
    ("enableDevice", "Nodes::enable_device"),
    ("streamEvents", "SencaClient::events"),
    ("createStream", "Streams::create"),
    ("listStreams", "Streams::list"),
    ("getStream", "Streams::get"),
    ("patchStream", "Streams::update"),
    ("deleteStream", "Streams::delete"),
    ("rotateStreamKey", "Streams::rotate_key"),
    ("createStreamPlaybackUrl", "Streams::playback_url"),
    ("startRecording", "Streams::start_recording"),
    ("planStream", "Streams::plan"),
    ("getStreamSnapshot", "Streams::snapshot"),
    ("listStreamSnapshots", "Streams::snapshots"),
    ("getStreamSnapshotById", "Streams::snapshot_by_id"),
    ("deleteStreamSnapshot", "Streams::delete_snapshot"),
    ("listRecordings", "Recordings::list"),
    ("getRecording", "Recordings::get"),
    ("patchRecording", "Recordings::update"),
    ("deleteRecording", "Recordings::delete"),
    ("stopRecording", "Recordings::stop"),
    ("createRecordingPlaybackUrl", "Recordings::playback_url"),
    ("getRecordingThumbnail", "Recordings::thumbnail"),
    ("exportRecordingMp4", "Recordings::export_mp4"),
    ("createSchedule", "Schedules::create"),
    ("listSchedules", "Schedules::list"),
    ("checkSchedule", "Schedules::check"),
    ("getSchedule", "Schedules::get"),
    ("patchSchedule", "Schedules::update"),
    ("deleteSchedule", "Schedules::delete"),
    ("listScheduleRuns", "Schedules::runs"),
    ("listAudit", "Audit::list"),
    ("createToken", "Tokens::create"),
    ("listTokens", "Tokens::list"),
    ("getToken", "Tokens::get"),
    ("deleteToken", "Tokens::delete"),
    ("getAuthProviders", "Auth::providers"),
    ("login", "Auth::login"),
    ("loginMfa", "Auth::login_mfa"),
    ("exchangeLoginCode", "Auth::exchange"),
    ("startOidcLogin", "Auth::oidc_start_url"),
    ("oidcCallback", "Auth::oidc_callback_url"),
    ("getMe", "Auth::me"),
    ("logout", "Auth::logout"),
    ("changePassword", "Auth::change_password"),
    ("setupTotp", "Auth::setup_totp"),
    ("confirmTotp", "Auth::confirm_totp"),
    ("disableMfa", "Auth::disable_mfa"),
    ("regenerateRecoveryCodes", "Auth::regenerate_recovery_codes"),
    ("listMySessions", "Auth::sessions"),
    ("revokeMySession", "Auth::revoke_session"),
    ("createUser", "Users::create"),
    ("listUsers", "Users::list"),
    ("getUser", "Users::get"),
    ("updateUser", "Users::update"),
    ("deleteUser", "Users::delete"),
    ("resetUserPassword", "Users::reset_password"),
    ("resetUserMfa", "Users::reset_mfa"),
    ("listUserSessions", "Users::sessions"),
    ("revokeUserSessions", "Users::revoke_sessions"),
    ("createInput", "Inputs::create"),
    ("listInputs", "Inputs::list"),
    ("getInput", "Inputs::get"),
    ("patchInput", "Inputs::update"),
    ("deleteInput", "Inputs::delete"),
    ("createOutput", "Outputs::create"),
    ("listOutputs", "Outputs::list"),
    ("getOutput", "Outputs::get"),
    ("patchOutput", "Outputs::update"),
    ("deleteOutput", "Outputs::delete"),
    ("createSecret", "Secrets::create"),
    ("listSecrets", "Secrets::list"),
    ("deleteSecret", "Secrets::delete"),
    ("createPipeline", "Pipelines::create"),
    ("listPipelines", "Pipelines::list"),
    ("getPipeline", "Pipelines::get"),
    ("patchPipeline", "Pipelines::update"),
    ("deletePipeline", "Pipelines::delete"),
    ("validatePipeline", "Pipelines::validate"),
    ("controlPipeline", "Pipelines::control"),
    ("listPipelineSnapshots", "Pipelines::snapshots"),
    ("getPipelineSnapshotById", "Pipelines::snapshot_by_id"),
    ("deletePipelineSnapshot", "Pipelines::delete_snapshot"),
    ("getLivePlaylist", "SencaClient::fetch_signed"),
    ("getLiveObject", "SencaClient::fetch_signed"),
    ("getVodPlaylist", "SencaClient::fetch_signed"),
    ("getVodObject", "SencaClient::fetch_signed"),
    ("createRoom", "Rooms::create"),
    ("listRooms", "Rooms::list"),
    ("getRoom", "Rooms::get"),
    ("patchRoom", "Rooms::update"),
    ("deleteRoom", "Rooms::delete"),
    ("issueRoomToken", "Rooms::issue_token"),
    ("postRoomMessage", "Rooms::send_message"),
    ("getRoomStats", "Rooms::stats"),
    ("listRoomParticipants", "Rooms::participants"),
    ("getRoomParticipant", "Rooms::participant"),
    ("patchRoomParticipant", "Rooms::update_participant"),
    ("disconnectRoomParticipant", "Rooms::disconnect"),
    ("createMediaPipeline", "MediaPipelines::create"),
    ("listMediaPipelines", "MediaPipelines::list"),
    ("getMediaPipeline", "MediaPipelines::get"),
    ("patchMediaPipeline", "MediaPipelines::update"),
    ("deleteMediaPipeline", "MediaPipelines::delete"),
    ("healthz", "SencaClient::healthz"),
    ("readyz", "SencaClient::readyz"),
];

#[cfg(test)]
mod tests {
    use super::OPERATIONS;
    use std::collections::BTreeSet;

    /// 仕様の operationId（YAML を読まず、`operationId:` の行を拾う）。
    fn spec_operation_ids() -> BTreeSet<String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api/openapi.yaml");
        let text = std::fs::read_to_string(path).expect("openapi.yaml");
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("operationId:"))
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .collect()
    }

    #[test]
    fn every_operation_has_a_method() {
        let spec = spec_operation_ids();
        let sdk: BTreeSet<String> = OPERATIONS.iter().map(|(o, _)| o.to_string()).collect();
        assert_eq!(sdk.len(), OPERATIONS.len(), "対応表に重複がある");
        let missing: Vec<_> = spec.difference(&sdk).collect();
        let stale: Vec<_> = sdk.difference(&spec).collect();
        assert!(missing.is_empty(), "SDK に無い操作（仕様に足された）: {missing:?}");
        assert!(stale.is_empty(), "仕様に無い操作（消えた・改名した）: {stale:?}");
    }

    /// 対応表のメソッドが実際にあること（型検査で確かめる）。
    #[allow(dead_code, clippy::let_underscore_future)]
    fn methods_exist(c: crate::SencaClient) {
        let _ = crate::SencaClient::capabilities;
        let _ = crate::SencaClient::devices;
        let _ = crate::SencaClient::grid;
        let _ = crate::SencaClient::events;
        let _ = crate::SencaClient::fetch_signed;
        let _ = crate::SencaClient::healthz;
        let _ = crate::SencaClient::readyz;
        let _ = (c.auth(), c.users(), c.with_token("sencas_x"));
        let _ = (crate::Auth::providers, crate::Auth::login, crate::Auth::login_mfa, crate::Auth::exchange, crate::Auth::oidc_start_url);
        let _ = (crate::Auth::oidc_callback_url, crate::Auth::me, crate::Auth::logout, crate::Auth::change_password, crate::Auth::setup_totp);
        let _ = (crate::Auth::confirm_totp, crate::Auth::disable_mfa, crate::Auth::regenerate_recovery_codes, crate::Auth::sessions);
        let _ = (crate::Auth::revoke_session, crate::Users::create, crate::Users::list, crate::Users::get);
        let _ = (crate::Users::delete, crate::Users::reset_password, crate::Users::reset_mfa, crate::Users::sessions, crate::Users::revoke_sessions);
        let _ = (c.streams(), c.recordings(), c.schedules(), c.tokens(), c.inputs(), c.outputs(), c.secrets(), c.pipelines(), c.nodes());
        let _ = (crate::Nodes::list, crate::Nodes::get, crate::Nodes::drain, crate::Nodes::undrain, crate::Nodes::disable_device, crate::Nodes::enable_device);
        let _ = (crate::Streams::create, crate::Streams::list, crate::Streams::get, crate::Streams::delete);
        let _ = (crate::Streams::rotate_key, crate::Streams::playback_url, crate::Streams::start_recording, crate::Streams::plan);
        let _ = (crate::Streams::snapshot, crate::Streams::snapshots, crate::Streams::snapshot_by_id, crate::Streams::delete_snapshot);
        let _ = (crate::Recordings::list, crate::Recordings::get, crate::Recordings::delete, crate::Recordings::stop);
        let _ = (crate::Recordings::playback_url, crate::Recordings::thumbnail, crate::Recordings::export_mp4);
        let _ = (crate::Schedules::create, crate::Schedules::list, crate::Schedules::check, crate::Schedules::get);
        let _ = (crate::Schedules::delete, crate::Schedules::runs);
        let _ = (crate::Tokens::create, crate::Tokens::list, crate::Tokens::get, crate::Tokens::delete);
        let _ = (crate::Inputs::create, crate::Inputs::list, crate::Inputs::get, crate::Inputs::delete);
        let _ = (crate::Outputs::create, crate::Outputs::list, crate::Outputs::get, crate::Outputs::delete);
        let _ = (crate::Secrets::create, crate::Secrets::list, crate::Secrets::delete);
        let _ = (crate::Pipelines::create, crate::Pipelines::list, crate::Pipelines::get, crate::Pipelines::delete);
        let _ = (crate::Pipelines::validate, crate::Pipelines::control);
        let _ = (crate::Pipelines::snapshots, crate::Pipelines::snapshot_by_id, crate::Pipelines::delete_snapshot);
        let _ = (crate::rooms::Rooms::create, crate::rooms::Rooms::list, crate::rooms::Rooms::get, crate::rooms::Rooms::delete);
        let _ = (crate::rooms::Rooms::issue_token, crate::rooms::Rooms::send_message, crate::rooms::Rooms::stats);
        let _ = (crate::rooms::Rooms::participants, crate::rooms::Rooms::participant, crate::rooms::Rooms::update_participant);
        let _ = crate::rooms::Rooms::disconnect;
        let _ = c.rooms().update("", &Default::default(), 1u64);
        let _ = (crate::MediaPipelines::create, crate::MediaPipelines::list, crate::MediaPipelines::get, crate::MediaPipelines::delete);
        let _ = c.media_pipelines().update("", &Default::default(), 1u64);
        let _ = (crate::Audit::list, crate::Audit::list_page);
        let _ = c.audit();
        let _ = c.media_pipelines().set_state("", crate::models::MediaPipelineState::Stopped, 1u64);
        // If-Match を impl Into で取る更新は呼び出しの形で確かめる
        let _ = c.streams().update("", &Default::default(), 1u64);
        let _ = c.recordings().update("", &Default::default(), 1u64);
        let _ = c.schedules().update("", &Default::default(), 1u64);
        let _ = c.inputs().update("", &Default::default(), 1u64);
        let _ = c.outputs().update("", &Default::default(), 1u64);
        let _ = c.pipelines().update("", &Default::default(), 1u64);
    }
}
