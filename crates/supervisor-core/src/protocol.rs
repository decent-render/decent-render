//! Render farm dispatch wire protocol, protocol version 2.
//!
//! Rust mirror of driffs `src/lib/render-farm/protocol.ts` — the
//! transport-agnostic message contract between the dispatch service and a
//! worker. The worker opens ONE outbound WebSocket to the platform; jobs are
//! pushed down, heartbeats ride the same connection (GitHub-Actions-runner
//! pattern — works behind any NAT, no public worker addresses).
//!
//! Field names are camelCase on the wire; messages are discriminated by the
//! `type` field. Every message carries `tenant` — driffs is tenant #1, but the
//! protocol is platform-agnostic by construction.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Must match the dispatch's `PROTOCOL_VERSION` (protocol.ts).
pub const PROTOCOL_VERSION: u32 = 2;

// ── Worker → server ─────────────────────────────────────────────────────────

/// Who operates this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// Company-run infrastructure (the platform's own fleet).
    Company,
    /// Third-party community operator.
    Community,
}

/// What this node can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub gpu: bool,
}

/// First message after connect — the worker introduces itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterMessage {
    pub tenant: String,
    pub protocol_version: u32,
    pub operator: Option<String>,
    pub platform: Platform,
    pub chip: String,
    pub ram_gb: u32,
    pub supervisor_version: String,
    pub payload_version: String,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatMessage {
    pub tenant: String,
    pub current_job_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobAcceptedMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgressMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Render progress in `[0, 1]`.
    pub progress: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobMetrics {
    /// Wall-clock render time on the worker, milliseconds.
    pub wall_ms: u64,
    pub frames: u64,
    /// Finished output size in bytes. Reported by the runner in its `done`
    /// event; dispatch persists it on the job row so the UI shows size without
    /// a second round-trip. `None` when the runner didn't report it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_size_in_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCompleteMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// R2 key the worker uploaded the finished output to (via presigned PUT).
    pub output_key: String,
    pub metrics: JobMetrics,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFailedMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    pub reason: String,
}

/// All worker → server messages, discriminated by the `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerMessage {
    Register(RegisterMessage),
    Heartbeat(HeartbeatMessage),
    JobAccepted(JobAcceptedMessage),
    JobProgress(JobProgressMessage),
    JobComplete(JobCompleteMessage),
    JobFailed(JobFailedMessage),
}

// ── Server → worker ─────────────────────────────────────────────────────────

/// Job class, used for dispatch routing (Lambda-able vs farm-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Standard,
    Gpu,
}

/// Output codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Vp8,
}

/// The protocol.ts `purgeAfter: z.literal(true)` — a boolean that is always
/// `true` on the wire. Deserialization rejects `false`, so a job that does not
/// carry the purge directive cannot even be parsed. This is the privacy rule
/// the supervisor exists to enforce (see [`crate::purge::WorkDir`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeAfter;

impl Serialize for PurgeAfter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for PurgeAfter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match bool::deserialize(deserializer)? {
            true => Ok(PurgeAfter),
            false => Err(D::Error::custom("purgeAfter must be the literal true")),
        }
    }
}

/// Job assignment. This is the privacy-rule carrier: assets arrive via
/// presigned R2 GET, output goes up via presigned PUT, and `purge_after`
/// directs the supervisor to wipe the working directory after the job. The
/// device only ever holds platform bundles + transient job assets — never
/// persisted user content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobAssignMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    pub kind: JobKind,
    pub duration_frames: u64,
    pub fps: u32,
    pub codec: Codec,
    /// Pinned platform bundle: content-addressed tar.gz of the Remotion
    /// webpack bundle. Download via the presigned GET, verify the sha256,
    /// extract, render against the extracted dir. Content-addressing makes the
    /// render reproducible (a redeploy can never mutate an in-flight job's
    /// bundle).
    pub bundle_sha256: String,
    pub bundle_get_url: String,
    /// Pinned render payload tarball (runner binary + remotion-binaries/).
    /// Dispatch resolves this by render_bundles.remotionVersion → active
    /// render_payloads row; the supervisor verifies and caches by sha.
    pub payload_sha256: String,
    pub payload_get_url: String,
    /// Presigned R2 GET for the job's input props JSON
    /// (`{compositionId, inputProps}`). Self-describing — the worker needs no
    /// other job data.
    pub input_props_get_url: String,
    /// Presigned R2 GET URLs for input assets.
    pub asset_get_urls: Vec<String>,
    /// Presigned R2 PUT URL the worker uploads the finished mp4 to.
    pub output_put_url: String,
    /// R2 key the output lands at (so the server can resolve it post-upload).
    pub output_key: String,
    /// Supervisor MUST purge the working directory after the job. Always true.
    pub purge_after: PurgeAfter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelMessage {
    pub tenant: String,
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingMessage {
    pub tenant: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailableMessage {
    pub tenant: String,
    pub supervisor_version: String,
    pub payload_version: String,
}

/// All server → worker messages, discriminated by the `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    JobAssign(JobAssignMessage),
    Cancel(CancelMessage),
    Ping(PingMessage),
    UpdateAvailable(UpdateAvailableMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    fn round_trip_worker(literal: &str) -> WorkerMessage {
        let parsed: WorkerMessage = serde_json::from_str(literal).expect("deserialize");
        let re: Value = serde_json::to_value(&parsed).expect("serialize");
        let orig: Value = serde_json::from_str(literal).unwrap();
        assert_eq!(re, orig, "round trip must preserve every field");
        parsed
    }

    fn round_trip_server(literal: &str) -> ServerMessage {
        let parsed: ServerMessage = serde_json::from_str(literal).expect("deserialize");
        let re: Value = serde_json::to_value(&parsed).expect("serialize");
        let orig: Value = serde_json::from_str(literal).unwrap();
        assert_eq!(re, orig, "round trip must preserve every field");
        parsed
    }

    /// The register frame exactly as spike-worker.ts sends it (onOpen).
    #[test]
    fn register_matches_spike_worker_frame() {
        let literal = r#"{"type":"register","tenant":"driffs","protocolVersion":2,"operator":null,"platform":"company","chip":"Apple M4 Max (darwin)","ramGb":64,"supervisorVersion":"spike-0.0.2","payloadVersion":"none","capabilities":{"gpu":true}}"#;
        let msg = round_trip_worker(literal);
        let WorkerMessage::Register(r) = msg else {
            panic!("expected register, got {msg:?}");
        };
        assert_eq!(r.tenant, "driffs");
        assert_eq!(r.protocol_version, PROTOCOL_VERSION);
        assert_eq!(r.operator, None);
        assert_eq!(r.platform, Platform::Company);
        assert!(r.capabilities.gpu);
    }

    #[test]
    fn heartbeat_matches_spike_worker_frame() {
        let literal = r#"{"type":"heartbeat","tenant":"driffs","currentJobCount":0}"#;
        let msg = round_trip_worker(literal);
        assert_eq!(
            msg,
            WorkerMessage::Heartbeat(HeartbeatMessage {
                tenant: "driffs".into(),
                current_job_count: 0,
            })
        );
    }

    #[test]
    fn job_lifecycle_worker_frames() {
        round_trip_worker(r#"{"type":"jobAccepted","tenant":"driffs","jobId":"spike-1"}"#);
        round_trip_worker(
            r#"{"type":"jobProgress","tenant":"driffs","jobId":"spike-1","progress":0.5}"#,
        );
        let complete = round_trip_worker(
            r#"{"type":"jobComplete","tenant":"driffs","jobId":"spike-1","outputKey":"renders/t1/out.mp4","metrics":{"wallMs":12345,"frames":300}}"#,
        );
        let WorkerMessage::JobComplete(c) = complete else {
            panic!("expected jobComplete");
        };
        assert_eq!(c.metrics.wall_ms, 12345);
        assert_eq!(c.metrics.frames, 300);
        assert_eq!(c.metrics.output_size_in_bytes, None);
        let complete_with_size = round_trip_worker(
            r#"{"type":"jobComplete","tenant":"driffs","jobId":"spike-1","outputKey":"renders/t1/out.mp4","metrics":{"wallMs":12345,"frames":300,"outputSizeInBytes":647399}}"#,
        );
        let WorkerMessage::JobComplete(c) = complete_with_size else {
            panic!("expected jobComplete");
        };
        assert_eq!(c.metrics.output_size_in_bytes, Some(647399));
        round_trip_worker(
            r#"{"type":"jobFailed","tenant":"driffs","jobId":"spike-1","reason":"bundle sha mismatch"}"#,
        );
    }

    /// jobAssign with every field from protocol.ts JobAssignMessageSchema.
    #[test]
    fn job_assign_matches_dispatch_shape() {
        let literal = r#"{"type":"jobAssign","tenant":"driffs","jobId":"job-render-abc123","kind":"gpu","durationFrames":300,"fps":30,"codec":"h264","bundleSha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","bundleGetUrl":"https://r2.example.com/bundles/9f86.tar.gz?sig=1","payloadSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","payloadGetUrl":"https://r2.example.com/render-payloads/aaaa.tar.gz?sig=payload","inputPropsGetUrl":"https://r2.example.com/renders/t1/input-props.json?sig=2","assetGetUrls":["https://r2.example.com/assets/a.png?sig=3"],"outputPutUrl":"https://r2.example.com/renders/t1/out.mp4?sig=4","outputKey":"renders/t1/out.mp4","purgeAfter":true}"#;
        let msg = round_trip_server(literal);
        let ServerMessage::JobAssign(a) = msg else {
            panic!("expected jobAssign");
        };
        assert_eq!(a.job_id, "job-render-abc123");
        assert_eq!(a.kind, JobKind::Gpu);
        assert_eq!(a.codec, Codec::H264);
        assert_eq!(a.duration_frames, 300);
        assert_eq!(a.fps, 30);
        assert_eq!(a.asset_get_urls.len(), 1);
        assert_eq!(a.purge_after, PurgeAfter);
    }

    /// protocol.ts: `purgeAfter: z.literal(true)` — false must not parse.
    #[test]
    fn job_assign_rejects_purge_after_false() {
        let mut v: Value = serde_json::from_str(
            r#"{"type":"jobAssign","tenant":"driffs","jobId":"j","kind":"standard","durationFrames":1,"fps":30,"codec":"vp8","bundleSha256":"x","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#,
        )
        .unwrap();
        assert!(serde_json::from_value::<ServerMessage>(v.clone()).is_ok());
        v["purgeAfter"] = json!(false);
        assert!(serde_json::from_value::<ServerMessage>(v).is_err());
    }

    /// Exact dispatch frame from driffs `src/dispatch/dispatcher.ts`:
    /// `conn.send({type: 'cancel', tenant: job.tenant, jobId: job.id})`.
    #[test]
    fn cancel_matches_dispatch_frame() {
        let literal = r#"{"type":"cancel","tenant":"driffs","jobId":"job-render-abc123"}"#;
        let msg = round_trip_server(literal);
        let ServerMessage::Cancel(c) = msg else {
            panic!("expected cancel");
        };
        assert_eq!(c.tenant, "driffs");
        assert_eq!(c.job_id, "job-render-abc123");
    }

    #[test]
    fn remaining_server_frames() {
        round_trip_server(r#"{"type":"ping","tenant":"driffs"}"#);
        round_trip_server(
            r#"{"type":"updateAvailable","tenant":"driffs","supervisorVersion":"rust-0.0.2","payloadVersion":"remotion-4.0.339"}"#,
        );
    }

    /// Cross-language conformance: every fixture in
    /// `packages/protocol/fixtures/v2.json` (the shared wire-format contract)
    /// must round-trip through the Rust types with no field drift. The TS
    /// package asserts the same fixtures against its zod schemas — so together
    /// the two sides cannot drift. The outputSizeInBytes scar is covered by the
    /// two jobComplete fixtures: if this struct ever dropped the field again,
    /// the PRESENT fixture would fail here, exactly as it failed in prod.
    #[test]
    fn cross_language_fixtures_round_trip() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/protocol/fixtures/v2.json"
        );
        let raw = fs::read_to_string(path)
            .expect("fixtures/v2.json must exist (run from the decent-render workspace)");
        let parsed: Value = serde_json::from_str(&raw).expect("fixtures are valid JSON");

        for case in parsed["cases"].as_array().expect("cases array") {
            let name = case["name"].as_str().unwrap();
            let direction = case["direction"].as_str().unwrap();
            let wire = case["wire"].clone();

            let re = match direction {
                "worker" => {
                    let msg: WorkerMessage =
                        serde_json::from_value(wire.clone()).expect("worker parse");
                    serde_json::to_value(&msg).expect("worker serialize")
                }
                "server" => {
                    let msg: ServerMessage =
                        serde_json::from_value(wire.clone()).expect("server parse");
                    serde_json::to_value(&msg).expect("server serialize")
                }
                other => panic!("unknown direction {other}"),
            };
            // Deep, order-independent structural equality (serde_json::Value
            // compares objects as maps): a missing or extra field on either side
            // makes the re-serialized value differ from the fixture.
            assert_eq!(re, wire, "fixture drifted: {name} ({direction})");
        }
    }
}
