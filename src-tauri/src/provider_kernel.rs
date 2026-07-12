use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyDecision {
    New,
    Replay,
    Conflict,
}

pub fn digest_decision(existing: Option<&str>, digest: &str) -> IdempotencyDecision {
    match existing {
        Some(value) if value == digest => IdempotencyDecision::Replay,
        Some(_) => IdempotencyDecision::Conflict,
        None => IdempotencyDecision::New,
    }
}

pub fn idempotency_decision(
    index: &mut BTreeMap<String, String>,
    key: &str,
    digest: &str,
) -> IdempotencyDecision {
    match digest_decision(index.get(key).map(String::as_str), digest) {
        IdempotencyDecision::New => {
            index.insert(key.to_string(), digest.to_string());
            IdempotencyDecision::New
        }
        decision => decision,
    }
}

pub fn terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled" | "lost")
}

pub fn transition_allowed(from: &str, to: &str) -> bool {
    if from == to {
        return !terminal_state(from);
    }
    matches!(
        (from, to),
        ("queued", "running")
            | ("queued", "waiting_input")
            | ("queued", "canceled")
            | ("queued", "failed")
            | ("queued", "lost")
            | ("waiting_input", "running")
            | ("waiting_input", "canceled")
            | ("waiting_input", "failed")
            | ("running", "waiting_input")
            | ("running", "completed")
            | ("running", "failed")
            | ("running", "canceled")
            | ("running", "lost")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantDecision {
    Consume,
    Replay,
    Exhausted,
}

pub fn grant_decision(
    uses: u32,
    max_uses: u32,
    last_request_digest: Option<&str>,
    request_digest: &str,
) -> GrantDecision {
    if last_request_digest == Some(request_digest) {
        GrantDecision::Replay
    } else if uses >= max_uses {
        GrantDecision::Exhausted
    } else {
        GrantDecision::Consume
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadWriteDecision {
    Claim,
    WaitForReplay,
    CompleteReplay,
    Conflict,
}

pub fn upload_write_decision(
    state: &str,
    stored_digest: Option<&str>,
    request_digest: &str,
) -> UploadWriteDecision {
    match state {
        "created" => UploadWriteDecision::Claim,
        "writing" if stored_digest == Some(request_digest) => UploadWriteDecision::WaitForReplay,
        "written" | "sealed" if stored_digest == Some(request_digest) => {
            UploadWriteDecision::CompleteReplay
        }
        _ => UploadWriteDecision::Conflict,
    }
}

pub fn callback_delay_seconds(attempt: u32, event_id: &str) -> u64 {
    let base = 2u64.saturating_pow(attempt.min(8));
    let mut hash = 0xcbf29ce484222325u64;
    for byte in event_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    base + hash % (base / 2 + 1)
}

pub fn callback_claimable(acknowledged: bool, lease_deadline_ms: Option<u64>, now_ms: u64) -> bool {
    !acknowledged
        && lease_deadline_ms
            .map(|deadline| deadline <= now_ms)
            .unwrap_or(true)
}

pub fn lifecycle_path(action: &str, instance_id: &str) -> Option<String> {
    match action {
        "register" => Some("/api/capability-providers/v1/register".to_string()),
        "heartbeat" => Some(format!("/api/capability-providers/v1/providers/venice-media-local/instances/{instance_id}/heartbeat")),
        "unregister" => Some(format!("/api/capability-providers/v1/providers/venice-media-local/instances/{instance_id}")),
        _ => None,
    }
}

pub fn next_heartbeat_sequence(current: i64) -> i64 {
    current.saturating_add(1)
}

pub fn may_create_missing_key(allow_create: bool, ledger_exists: bool) -> bool {
    allow_create && !ledger_exists
}

pub fn execution_claim_available(
    current_owner: Option<&str>,
    lease_expires_ms: Option<i64>,
    requested_owner: &str,
    now_ms: i64,
) -> bool {
    current_owner == Some(requested_owner)
        || lease_expires_ms
            .map(|expires| expires <= now_ms)
            .unwrap_or(true)
}

pub fn heartbeat_replay_valid(
    persisted_digest: &str,
    recomputed_digest: &str,
    persisted_sequence: i64,
    body_sequence: Option<i64>,
) -> bool {
    persisted_digest == recomputed_digest && body_sequence == Some(persisted_sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn concurrent_admission_has_one_new_owner() {
        let index = Arc::new(Mutex::new(BTreeMap::new()));
        let mut workers = Vec::new();
        for _ in 0..16 {
            let index = Arc::clone(&index);
            workers.push(thread::spawn(move || {
                idempotency_decision(&mut index.lock().unwrap(), "client:key", "digest")
            }));
        }
        let decisions = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            decisions
                .iter()
                .filter(|value| **value == IdempotencyDecision::New)
                .count(),
            1
        );
        assert_eq!(
            decisions
                .iter()
                .filter(|value| **value == IdempotencyDecision::Replay)
                .count(),
            15
        );
    }

    #[test]
    fn upload_replay_has_one_writer() {
        assert_eq!(
            upload_write_decision("created", None, "d"),
            UploadWriteDecision::Claim
        );
        assert_eq!(
            upload_write_decision("writing", Some("d"), "d"),
            UploadWriteDecision::WaitForReplay
        );
        assert_eq!(
            upload_write_decision("sealed", Some("d"), "d"),
            UploadWriteDecision::CompleteReplay
        );
        assert_eq!(
            upload_write_decision("sealed", Some("d"), "other"),
            UploadWriteDecision::Conflict
        );
    }

    #[test]
    fn consumable_grant_replays_exactly_and_exhausts() {
        assert_eq!(
            grant_decision(0, 1, None, "request-a"),
            GrantDecision::Consume
        );
        assert_eq!(
            grant_decision(1, 1, Some("request-a"), "request-a"),
            GrantDecision::Replay
        );
        assert_eq!(
            grant_decision(1, 1, Some("request-a"), "request-b"),
            GrantDecision::Exhausted
        );
    }

    #[test]
    fn cancellation_control_replays_only_the_same_digest() {
        assert_eq!(digest_decision(None, "cancel-a"), IdempotencyDecision::New);
        assert_eq!(
            digest_decision(Some("cancel-a"), "cancel-a"),
            IdempotencyDecision::Replay
        );
        assert_eq!(
            digest_decision(Some("cancel-a"), "cancel-b"),
            IdempotencyDecision::Conflict
        );
    }

    #[test]
    fn terminal_states_are_monotonic() {
        assert!(transition_allowed("running", "completed"));
        for terminal in ["completed", "failed", "canceled", "lost"] {
            assert!(!transition_allowed(terminal, "running"));
            assert!(!transition_allowed(terminal, terminal));
        }
    }

    #[test]
    fn callback_jitter_is_deterministic_and_bounded() {
        let delay = callback_delay_seconds(4, "event-a");
        assert_eq!(delay, callback_delay_seconds(4, "event-a"));
        assert!((16..=24).contains(&delay));
        assert!(callback_claimable(false, None, 10));
        assert!(!callback_claimable(false, Some(11), 10));
        assert!(callback_claimable(false, Some(10), 10));
        assert!(!callback_claimable(true, None, 10));
    }

    #[test]
    fn fake_core_lifecycle_is_scoped_and_monotonic() {
        let instance = "instance-test-1";
        assert_eq!(
            lifecycle_path("register", instance).unwrap(),
            "/api/capability-providers/v1/register"
        );
        assert!(lifecycle_path("heartbeat", instance)
            .unwrap()
            .ends_with("/instance-test-1/heartbeat"));
        assert!(lifecycle_path("unregister", instance)
            .unwrap()
            .ends_with("/instance-test-1"));
        assert_eq!(next_heartbeat_sequence(-1), 0);
        assert_eq!(next_heartbeat_sequence(0), 1);
    }

    #[test]
    fn fake_core_accepts_lifecycle_http_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let server_observed = Arc::clone(&observed);
        let server = thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let count = stream.read(&mut request).unwrap();
                let first_line = String::from_utf8_lossy(&request[..count])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                server_observed.lock().unwrap().push(first_line);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
        });
        let instance = "fake-instance";
        for (method, action) in [
            ("POST", "register"),
            ("POST", "heartbeat"),
            ("DELETE", "unregister"),
        ] {
            let path = lifecycle_path(action, instance).unwrap();
            let mut stream = TcpStream::connect(address).unwrap();
            let request = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer synthetic-scoped-credential\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK"));
        }
        server.join().unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(
            observed[0],
            "POST /api/capability-providers/v1/register HTTP/1.1"
        );
        assert!(observed[1].ends_with("/fake-instance/heartbeat HTTP/1.1"));
        assert!(observed[2].starts_with("DELETE "));
    }

    #[test]
    fn missing_ledger_key_never_replaces_existing_evidence() {
        assert!(may_create_missing_key(true, false));
        assert!(!may_create_missing_key(true, true));
        assert!(!may_create_missing_key(false, false));
    }

    #[test]
    fn execution_claim_is_compare_and_swap_with_expiry() {
        assert!(!execution_claim_available(
            Some("owner-a"),
            Some(101),
            "owner-b",
            100
        ));
        assert!(execution_claim_available(
            Some("owner-a"),
            Some(100),
            "owner-b",
            100
        ));
        assert!(execution_claim_available(
            Some("owner-a"),
            Some(101),
            "owner-a",
            100
        ));
    }

    #[test]
    fn heartbeat_replay_requires_exact_sequence_and_digest() {
        assert!(heartbeat_replay_valid("digest", "digest", 4, Some(4)));
        assert!(!heartbeat_replay_valid("digest", "other", 4, Some(4)));
        assert!(!heartbeat_replay_valid("digest", "digest", 4, Some(5)));
    }
}
