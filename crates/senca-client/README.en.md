# senca-client (Rust client for the management API)

English | [日本語](README.md)

An asynchronous client for the SENCA management API (`/api/v1`, [`docs/api/openapi.yaml`](../../docs/api/openapi.yaml)) for use with tokio.
HTTP/1.1 comes from `senca-http` and TLS from `senca-tls`; it does not rely on an external HTTP client (the external dependencies are
tokio, serde, serde_json, bytes, getrandom and `futures-core`).

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, RequestOptions, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

// Create (an Idempotency-Key is added to POST automatically; you can also set one)
let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone(); // the stream key is only returned in this response

// Updates require If-Match. The previous response (its ETag) can be passed as is
let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* read again and retry */ todo!() }
    Err(e) => return Err(e.into()),
};

// Listing yields items by following cursors (also a futures::Stream)
let mut it = client.streams().list_with(ListOptions::default().limit(100));
while let Some(x) = it.next().await {
    println!("{}", x?.name);
}

// SSE (reconnects with Last-Event-ID when cut)
let mut events = client.events(EventsOptions::default().types(["stream.*", "recording.*"]));
while let Some(ev) = events.next().await {
    match ev? {
        ServerEvent::Event(e) => println!("{} {} {}", e.event, e.data.resource_id, e.data.occurred_at),
        ServerEvent::Resync { .. } => { /* events may have been missed: reload the lists */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

## Operations

Each resource is reached through `client.<resource>()` (the mapping between methods and openapi.yaml operationIds is `OPERATIONS` in
`src/ops.rs`; a test checks it against the specification and fails when the specification gains an operation the SDK lacks).

| Resource | Methods |
|---|---|
| `streams()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `rotate_key`, `playback_url`, `start_recording`, `plan`, `snapshot`, `snapshots`, `snapshot_by_id`, `delete_snapshot` |
| `recordings()` | `list`, `list_with` (filter with `ListOptions::stream`), `list_page`, `get`, `update`, `delete`, `stop`, `playback_url`, `thumbnail`, `export_mp4` |
| `schedules()` | `create`, `list`, `check`, `get`, `update`, `delete`, `runs` |
| `tokens()` | `create`, `list`, `get`, `delete` |
| `inputs()`, `outputs()` | `create(stream_id, ..)`, `list(stream_id)`, `get`, `update`, `delete` |
| `secrets()` | `create`, `list`, `delete` |
| `pipelines()` | `create`, `list`, `get`, `update`, `delete`, `validate`, `control` |
| `nodes()` | `list`, `get`, `drain`, `undrain`, `disable_device`, `enable_device` |
| Client | `capabilities`, `grid`, `devices`, `events`, `healthz`, `readyz`, `fetch_signed` (signed playback paths, follows 307), `request_json` (untyped requests) |
| `rooms()` | `create`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `issue_token`, `send_message`, `stats`, `participants`, `participants_with`, `participant`, `update_participant`, `disconnect` |
| `media_pipelines()` | `create`, `list`, `list_with` (filter with `ListOptions::room`), `list_page`, `get`, `update`, `set_state`, `delete` |
| `auth()` | `providers`, `login` (`LoginOutcome::Ok` or `MfaRequired`), `login_mfa`, `exchange`, `oidc_start_url`, `oidc_callback_url`, `me`, `logout`, `change_password`, `setup_totp`, `confirm_totp`, `disable_mfa`, `regenerate_recovery_codes`, `sessions`, `revoke_session`. The login operations work on a client without a token (`SencaClient::builder(url).build()`); use `with_token` with the session value a login returns |
| `users()` | `create`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `reset_password`, `reset_mfa`, `sessions`, `revoke_sessions` |

## Conventions

- **Versions**: `get`, `create` and `update` return `Versioned<T>` (`value`, `etag`, `replayed`, `request_id`; `Deref` gives the inner value).
  The `if_match` of `update(id, patch, if_match)` accepts `&Versioned<T>`, `IfMatch::Etag`, `IfMatch::Generation(n)` or a `u64`.
- **Idempotency**: every POST gets a fresh key (`sdk-<128 bit>`), and retries reuse the same key. If you choose the key with
  `RequestOptions::idempotency_key("..")`, resending even from another process gives the same answer (`Versioned::replayed` is true).
- **Errors**: `Error::Api(ApiError)` carries the problem+json `code` (`ProblemCode`; unknown values become `Unknown` with `raw_code`), `title`,
  `status`, `retryable`, `request_id`, `details` and `retry_after`. The other variants are `Connect`, `Transport`, `Timeout`, `Decode`,
  `InvalidUrl` and `InvalidArgument`.
- **Retries** (`RetryPolicy`: up to 3 retries by default, starting at 200 ms and doubling up to 10 seconds, ±20 % jitter, never shorter than `Retry-After`):
  requests that could not connect; timeouts and send/receive errors of idempotent requests (GET, DELETE and POST with an Idempotency-Key);
  and responses with `retryable: true` or 429, 502, 503, 504. PATCH is retried only when the server says it had no effect (`retryable`, 429, 503).
- **Models**: `src/models.rs` is generated from openapi.yaml (`python3 crates/senca-client/gen/gen_models.py`). Unknown fields in responses
  are skipped and unknown enum values become `Unknown` (so the client keeps working when the server is updated first). Nullable fields of
  `*Patch` are `Option<Option<T>>` (`None` = leave unchanged, `Some(None)` = clear with null).
- **TLS**: the OS certificate authorities by default. They can be replaced with `SencaClient::builder(url).root_certificates_pem(pem)`.

## Tests

```
CARGO_TARGET_DIR=... cargo test -p senca-client
```

There are unit tests, tests against a mock server (`tests/mock.rs`) and tests against a real senca-node (`tests/node.rs`).
What each of them checks is described in [docs/dev/notes/senca-client-tests.md](../../docs/dev/notes/senca-client-tests.md).
