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

/// `register.protocolVersion` is pinned, not ranged: this crate speaks exactly
/// [`PROTOCOL_VERSION`], and a frame announcing any other version fails to
/// parse instead of being treated as v2. The TS side pins with
/// `z.literal(PROTOCOL_VERSION)`; the `reject` fixtures hold both to it.
fn pinned_protocol_version<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(deserializer)?;
    if v == PROTOCOL_VERSION {
        Ok(v)
    } else {
        Err(D::Error::custom(format!(
            "unsupported protocolVersion {v}; this node speaks {PROTOCOL_VERSION}"
        )))
    }
}

/// `jobProgress.progress` is a fraction in `[0, 1]` (TS: `.min(0).max(1)`).
/// Anything else is a runner bug, and a bug should fail to parse rather than
/// travel the wire as a number. Also reused for the runner-stdout contract's
/// `progress` event (runner.rs RunnerEvent) — same bound, same rationale:
/// dispatch's schema refuses out-of-range progress, so it must not leave the
/// supervisor in the first place.
pub(crate) fn unit_interval<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let v = f64::deserialize(deserializer)?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(D::Error::custom(format!(
            "progress must be in [0, 1], got {v}"
        )))
    }
}

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
///
/// A statement about HARDWARE, not about willingness. `gpu` used to be wired to
/// the operator's "accept real jobs" toggle, which meant a Raspberry-class node
/// with the switch on advertised itself as GPU-capable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Can this node render the GPU path (`chromiumOptions: {gl: 'angle'}`)?
    pub gpu: bool,
    /// How many jobs this node will run at once.
    ///
    /// Optional so frames from supervisors predating this field still parse;
    /// dispatch treats `None` as 1, which is what it always assumed anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<u32>,
    /// `std::env::consts::OS` — "macos", "linux".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// `std::env::consts::ARCH` — "aarch64", "x86_64".
    ///
    /// Reported so dispatch can stop handing a darwin-arm64 payload to a Linux
    /// box once the platform matrix lands. Nothing consumes it for routing yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

/// First message after connect — the worker introduces itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterMessage {
    pub tenant: String,
    #[serde(deserialize_with = "pinned_protocol_version")]
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
    #[serde(deserialize_with = "unit_interval")]
    pub progress: f64,
    /// ACCRUED-COST PROTOCOL (F2) — RAW MEASUREMENTS, never money.
    ///
    /// Integer ms since render start. A new runner reports how long it has
    /// been rendering and how many frames it has finished; DISPATCH prices
    /// those with its own rate card to persist mid-flight accrued cost.
    /// The runner cannot price its own work — pricing is the platform's private
    /// concern and lives nowhere in this crate. Optional so every node
    /// predating these fields stays a fully functional v2 peer (no
    /// `PROTOCOL_VERSION` bump: the pin is a register-time gate that would
    /// orphan every existing node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Integer frames finished so far (derived progress × duration). Same
    /// contract as [`Self::elapsed_ms`]: raw measurement, integer ≥ 0,
    /// optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_so_far: Option<u64>,
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

/// Why a worker declined an assignment it never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RejectReason {
    /// The operator has not opted in to executing real jobs.
    NotAccepting,
    /// A job is already in flight on this node.
    Busy,
}

/// CANCEL-ACK WITNESS (F4) — the node→dispatch acknowledgement of a
/// dispatch-initiated cancel, emitted in `connection.rs` at the point where
/// the terminal frame is suppressed, i.e. AFTER the job task (teardown +
/// purge) is joined — never on cancel-frame receipt ("cancel accepted" is
/// not "job stopped", FARM-1 §1-C).
///
/// `attempt` is REQUIRED — the provenance key. Dispatch persists it beside
/// the ack timestamp and its write predicate is idempotent by
/// (job_id, attempt); a requeued job can never inherit an earlier attempt's
/// ack. A lease assigned without an attempt is acked as attempt 1, matching
/// what dispatch's `assignmentAttempt` already assumes.
///
/// `teardown_ms` is the witness measurement: integer ms from cancel receipt
/// to process-tree dead + purge done. Optional (int ≥ 0 on the wire; a
/// negative value fails to parse on both sides — `u64` here, the reject
/// fixture pins the TS side).
///
/// Additive frame, [`PROTOCOL_VERSION`] stays 2: an old dispatch ignores an
/// unknown `type` (its inbound-frames policy), an old node never sends this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCanceledAckMessage {
    pub tenant: String,
    pub job_id: String,
    /// REQUIRED (unlike the terminal frames' optional lease attempt).
    pub attempt: u32,
    /// Integer ms from cancel receipt to process tree dead + purge done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown_ms: Option<u64>,
}

/// A job was declined **without being started** — distinct from
/// [`JobFailedMessage`], which reports a render that ran and failed.
///
/// Dispatch should requeue immediately without counting an attempt: nothing was
/// consumed and no work was lost. Before this existed, a refusing node stayed
/// silent and dispatch hard-failed the job after `max(10min, expected × 20)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRejectedMessage {
    pub tenant: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    pub reason: RejectReason,
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
    JobRejected(JobRejectedMessage),
    JobCanceledAck(JobCanceledAckMessage),
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

/// The format of a still render (`jobAssign.still.format`). Closed set: the
/// certification stills this directive carries are lossless PNGs by contract;
/// a lossy or unknown format is a parse error, not a runtime surprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StillFormat {
    Png,
}

/// STILL RENDER DIRECTIVE (FARM-STILL) — `jobAssign.still`. Present when the
/// job renders exactly ONE frame as a lossless PNG (`renderStill`) instead of
/// a video (`renderMedia`); absent means today's video job, byte-identical
/// behaviour. `frame` is the zero-based frame index; it must be
/// `< durationFrames`, which [`JobAssignMessage`]'s `Deserialize` enforces at
/// parse time (the TS side enforces it with a cross-field refine — neither
/// side may accept a still outside the composition).
///
/// Additive-optional field: NO `PROTOCOL_VERSION` bump (old peers ignore the
/// unknown field; the version pin is a register-time gate that would orphan
/// the fleet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillDirective {
    pub frame: u64,
    pub format: StillFormat,
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
///
/// Deserialize is HAND-WRITTEN (over [`JobAssignMessageUnchecked`]) solely to
/// enforce the cross-field still bound (`still.frame < duration_frames`) at
/// parse time; Serialize stays derived. Every field must appear in BOTH
/// structs — the shared fixtures round-trip the checked struct, so a field
/// missing from either side fails `cross_language_fixtures_round_trip`.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    /// Pinned browser tarball, cached separately from the payload under
    /// `~/.decent-worker/browsers/<sha>`.
    ///
    /// The browser is ~170MB and identical across Remotion versions pinning the
    /// same Chrome build; splitting it out of the payload means it is fetched
    /// once per Chrome version rather than once per (Remotion version,
    /// platform). The tarball root holds an `executable` file naming the
    /// browser's path relative to that root.
    ///
    /// Optional: payloads that still bundle their own browser under `chrome/`
    /// omit it, and the runner falls back to that in-payload manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_get_url: Option<String>,
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
    /// STILL RENDER DIRECTIVE (FARM-STILL) — optional; absent ⇒ video job.
    /// See [`StillDirective`] for the contract and the parse-time bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still: Option<StillDirective>,
}

/// Derive-only mirror of [`JobAssignMessage`] used to deserialize without the
/// cross-field check, which then runs in the hand-written `Deserialize` below.
/// Keep the field list IDENTICAL to the checked struct (fixture round-trip
/// pins it).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobAssignMessageUnchecked {
    pub tenant: String,
    pub job_id: String,
    #[serde(default)]
    pub attempt: Option<u32>,
    pub kind: JobKind,
    pub duration_frames: u64,
    pub fps: u32,
    pub codec: Codec,
    pub bundle_sha256: String,
    pub bundle_get_url: String,
    pub payload_sha256: String,
    pub payload_get_url: String,
    #[serde(default)]
    pub browser_sha256: Option<String>,
    #[serde(default)]
    pub browser_get_url: Option<String>,
    pub input_props_get_url: String,
    pub asset_get_urls: Vec<String>,
    pub output_put_url: String,
    pub output_key: String,
    pub purge_after: PurgeAfter,
    #[serde(default)]
    pub still: Option<StillDirective>,
}

impl TryFrom<JobAssignMessageUnchecked> for JobAssignMessage {
    type Error = String;

    fn try_from(u: JobAssignMessageUnchecked) -> Result<Self, Self::Error> {
        if let Some(still) = &u.still {
            if still.frame >= u.duration_frames {
                return Err(format!(
                    "still.frame {} must be < durationFrames {}",
                    still.frame, u.duration_frames
                ));
            }
        }
        Ok(Self {
            tenant: u.tenant,
            job_id: u.job_id,
            attempt: u.attempt,
            kind: u.kind,
            duration_frames: u.duration_frames,
            fps: u.fps,
            codec: u.codec,
            bundle_sha256: u.bundle_sha256,
            bundle_get_url: u.bundle_get_url,
            payload_sha256: u.payload_sha256,
            payload_get_url: u.payload_get_url,
            browser_sha256: u.browser_sha256,
            browser_get_url: u.browser_get_url,
            input_props_get_url: u.input_props_get_url,
            asset_get_urls: u.asset_get_urls,
            output_put_url: u.output_put_url,
            output_key: u.output_key,
            purge_after: u.purge_after,
            still: u.still,
        })
    }
}

impl<'de> Deserialize<'de> for JobAssignMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let unchecked = JobAssignMessageUnchecked::deserialize(deserializer)?;
        Self::try_from(unchecked).map_err(DeError::custom)
    }
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
    /// Boxed because it dwarfs every other variant — an unboxed jobAssign makes
    /// each `ServerMessage`, including a two-field ping, cost its full size.
    JobAssign(Box<JobAssignMessage>),
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

    /// PROTOCOL_VERSION stays pinned at 2 — additive fields, never a bump
    /// (F2 accrued-cost protocol / inspection-I10 §1.4).
    ///
    /// The pin is a REGISTER-TIME gate, not a capability signal:
    /// `pinned_protocol_version` (and TS's `z.literal(PROTOCOL_VERSION)`)
    /// make a node announcing any other version fail the register parse
    /// entirely — an old node speaking v2 to a v3 dispatch becomes an
    /// UNREGISTERED socket, never assigned, never told why. A bump therefore
    /// orphans every existing node at register. Additive-optional fields
    /// (jobProgress's elapsedMs/framesSoFar) are wire-tolerant in BOTH
    /// directions — serde ignores unknown fields here, zod strips them on
    /// dispatch — which is exactly why that change needed NO bump. If this
    /// test fails, you changed the wire in a way that breaks old nodes;
    /// either revert to an additive shape or make the fleet-orphaning an
    /// explicit, human-gated decision instead of editing the test.
    #[test]
    fn protocol_version_stays_pinned_at_2() {
        assert_eq!(PROTOCOL_VERSION, 2);
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

    /// FARM-STILL: a still jobAssign round-trips WITH the directive, and a
    /// video jobAssign stays byte-identical (no `still` key is emitted).
    #[test]
    fn job_assign_still_round_trips_and_video_stays_unchanged() {
        let video = r#"{"type":"jobAssign","tenant":"driffs","jobId":"j","kind":"standard","durationFrames":300,"fps":30,"codec":"h264","bundleSha256":"x","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true}"#;
        let msg = round_trip_server(video);
        let ServerMessage::JobAssign(a) = msg else {
            panic!("expected jobAssign");
        };
        assert_eq!(a.still, None);

        let still_wire = "{\"type\":\"jobAssign\",\"tenant\":\"driffs\",\"jobId\":\"j\",\"kind\":\"standard\",\"durationFrames\":300,\"fps\":30,\"codec\":\"h264\",\"bundleSha256\":\"x\",\"bundleGetUrl\":\"u\",\"payloadSha256\":\"p\",\"payloadGetUrl\":\"u\",\"inputPropsGetUrl\":\"u\",\"assetGetUrls\":[],\"outputPutUrl\":\"u\",\"outputKey\":\"renders/t1/still-f12.png\",\"purgeAfter\":true,\"still\":{\"frame\":12,\"format\":\"png\"}}".to_string();
        let msg = round_trip_server(&still_wire);
        let ServerMessage::JobAssign(a) = msg else {
            panic!("expected jobAssign");
        };
        assert_eq!(
            a.still,
            Some(StillDirective {
                frame: 12,
                format: StillFormat::Png
            })
        );
    }

    /// FARM-STILL cross-field bound: a still at or beyond the composition's
    /// duration is unrenderable and must fail at PARSE time, exactly like the
    /// TS side's refine. This is the Deserialize that delegates through
    /// JobAssignMessageUnchecked.
    #[test]
    fn job_assign_rejects_still_frame_at_or_past_duration() {
        let frame = |f: u64| {
            format!(
                "{{\"type\":\"jobAssign\",\"tenant\":\"driffs\",\"jobId\":\"j\",\"kind\":\"standard\",\"durationFrames\":300,\"fps\":30,\"codec\":\"h264\",\"bundleSha256\":\"x\",\"bundleGetUrl\":\"u\",\"payloadSha256\":\"p\",\"payloadGetUrl\":\"u\",\"inputPropsGetUrl\":\"u\",\"assetGetUrls\":[],\"outputPutUrl\":\"u\",\"outputKey\":\"k\",\"purgeAfter\":true,\"still\":{{\"frame\":{f},\"format\":\"png\"}}}}"
            )
        };
        // At the boundary and past it: refused.
        assert!(serde_json::from_str::<ServerMessage>(&frame(300)).is_err());
        assert!(serde_json::from_str::<ServerMessage>(&frame(301)).is_err());
        // One inside: accepted.
        assert!(serde_json::from_str::<ServerMessage>(&frame(299)).is_ok());
    }

    /// FARM-STILL: the format set is closed — a lossy/unknown format must not
    /// parse (the certification contract is lossless PNG only).
    #[test]
    fn job_assign_rejects_unknown_still_format() {
        let wire = r#"{"type":"jobAssign","tenant":"driffs","jobId":"j","kind":"standard","durationFrames":300,"fps":30,"codec":"h264","bundleSha256":"x","bundleGetUrl":"u","payloadSha256":"p","payloadGetUrl":"u","inputPropsGetUrl":"u","assetGetUrls":[],"outputPutUrl":"u","outputKey":"k","purgeAfter":true,"still":{"frame":12,"format":"jpeg"}}"#;
        assert!(serde_json::from_str::<ServerMessage>(wire).is_err());
    }

    /// Exact dispatch cancel frame (fixture-pinned shape; the frame is
    /// `conn.send({type: 'cancel', tenant: job.tenant, jobId: job.id})`).
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

    /// F4 cancel-ack: the node→dispatch acknowledgement of a cancel, sent
    /// after the job task (teardown + purge) is joined. `attempt` is REQUIRED
    /// (the provenance key — missing-attempt and string-attempt are reject
    /// fixtures), `teardown_ms` optional (the shared fixtures carry both
    /// shapes; negative is a reject fixture — `u64` rejects it at parse).
    #[test]
    fn job_canceled_ack_shape() {
        let with_teardown = round_trip_worker(
            r#"{"type":"jobCanceledAck","tenant":"driffs","jobId":"spike-1","attempt":2,"teardownMs":812}"#,
        );
        let WorkerMessage::JobCanceledAck(a) = with_teardown else {
            panic!("expected jobCanceledAck");
        };
        assert_eq!(a.attempt, 2);
        assert_eq!(a.teardown_ms, Some(812));
        assert_eq!(a.tenant, "driffs");

        let without = round_trip_worker(
            r#"{"type":"jobCanceledAck","tenant":"driffs","jobId":"spike-1","attempt":1}"#,
        );
        let WorkerMessage::JobCanceledAck(b) = without else {
            panic!("expected jobCanceledAck");
        };
        assert_eq!(b.teardown_ms, None);

        // attempt is required: a missing field must not parse (serde's
        // missing-field error, not a default).
        assert!(serde_json::from_str::<WorkerMessage>(
            r#"{"type":"jobCanceledAck","tenant":"driffs","jobId":"spike-1","teardownMs":812}"#
        )
        .is_err());
        // A string attempt is not a lease number.
        assert!(serde_json::from_str::<WorkerMessage>(
            r#"{"type":"jobCanceledAck","tenant":"driffs","jobId":"spike-1","attempt":"1"}"#
        )
        .is_err());
        // u64 rejects a negative teardown measurement.
        assert!(serde_json::from_str::<WorkerMessage>(
            r#"{"type":"jobCanceledAck","tenant":"driffs","jobId":"spike-1","attempt":1,"teardownMs":-5}"#
        )
        .is_err());
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

        // Teeth (C-2): an empty set iterates zero times and passes vacuously.
        let cases = parsed["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty(), "positive fixture set must be non-empty");

        for case in cases {
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

    /// Negative contract: every entry in the fixture file's `reject` array
    /// must FAIL to parse. TS asserts the same entries against its zod
    /// schemas, so neither side can quietly loosen a bound (protocolVersion
    /// pin, progress range, codec set) without the other side noticing.
    #[test]
    fn cross_language_fixtures_reject() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/protocol/fixtures/v2.json"
        );
        let raw = fs::read_to_string(path).expect("fixtures/v2.json must exist");
        let parsed: Value = serde_json::from_str(&raw).expect("fixtures are valid JSON");
        let reject = parsed["reject"].as_array().expect("reject array");
        assert!(!reject.is_empty(), "negative fixture set must be non-empty");

        // Collect every accepted entry so one run names ALL the loose bounds.
        let mut accepted = Vec::new();
        for case in reject {
            let name = case["name"].as_str().unwrap();
            let direction = case["direction"].as_str().unwrap();
            let reason = case["reason"].as_str().unwrap_or("");
            let wire = case["wire"].clone();
            let ok = match direction {
                "worker" => serde_json::from_value::<WorkerMessage>(wire).is_ok(),
                "server" => serde_json::from_value::<ServerMessage>(wire).is_ok(),
                other => panic!("unknown direction {other}"),
            };
            if ok {
                accepted.push(format!("{name} ({direction}) -- {reason}"));
            }
        }
        assert!(
            accepted.is_empty(),
            "negative fixtures were ACCEPTED:\n  {}",
            accepted.join("\n  ")
        );
    }
}
