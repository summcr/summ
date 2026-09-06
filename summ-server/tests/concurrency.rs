//! Race conditions on the live paths, against the real storage stack.
//!
//! `wiring.rs` proves that what was pushed comes back. This file asks the
//! question that a single-threaded suite structurally cannot: whether it still
//! comes back while somebody else is pushing. Every read path in summ is a
//! sequence of independent `get`s - `MetaEngine` has no snapshot, by design -
//! so `GET /v2/<name>/manifests/<tag>` is three separate lookups (`T`, then
//! `M`, then `B`) with a writer free to commit between any two of them. That is
//! not a bug in itself; it is the property whose consequences have to be
//! pinned.
//!
//! # The shape of a scenario
//!
//! Each test runs one or more writers and a handful of readers against one
//! `Backend` on a `TempDir`, for a wall-clock budget, on a multi-threaded
//! runtime. A reader never asserts inline: it returns the violations it saw, so
//! a failure names the interleaving instead of arriving as `task panicked`.
//!
//! The assertions are chosen so that a *correct* registry passes at any
//! interleaving, which is what makes them worth running repeatedly. In
//! particular "the response was one of the things we pushed" is deliberately
//! not the bar - a badly stale read passes it. Where an ordering is genuinely
//! guaranteed, the test asserts the ordering.
//!
//! # Running it longer
//!
//! The default budget is small enough that `cargo test` stays a gate rather
//! than a wait. Three environment variables turn the same file into a soak,
//! which is what to point at a release candidate:
//!
//! ```text
//! SUMM_STRESS_SECS=60 SUMM_STRESS_WIDTH=16 cargo test --test concurrency -- --nocapture
//! ```
//!
//! `SUMM_STRESS_SECS` is the per-scenario budget in seconds (default 0.5),
//! `SUMM_STRESS_WIDTH` the number of concurrent readers (default 6), and
//! `SUMM_STRESS_SEED` seeds the jitter that decides interleavings (default
//! fixed, so an ordinary run is reproducible). `--nocapture` is worth having:
//! several tests print what they observed as well as what they assert, because
//! a window that is real but rarely hit is a number rather than a failure.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use sha2::{Digest as _, Sha256};
use summ_registry::RegistryOptions;
use summ_server::backend::{Backend, Engine};
use summ_server::config::ServerConfig;
use summ_server::counters::PullCounters;
use summ_server::{router, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

// ------------------------------------------------------------------ knobs --

/// Per-scenario wall-clock budget.
///
/// Small by default: seven scenarios at half a second each is a few seconds of
/// `cargo test`, which is the price at which a concurrency suite actually gets
/// run before every release rather than being remembered afterwards.
fn budget() -> Duration {
    let secs: f64 = std::env::var("SUMM_STRESS_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.5);
    Duration::from_secs_f64(secs)
}

/// Concurrent readers per scenario.
fn width() -> usize {
    std::env::var("SUMM_STRESS_WIDTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
}

fn seed() -> u64 {
    std::env::var("SUMM_STRESS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// splitmix64, so the jitter is deterministic without a `rand` dependency in
/// the dev tree. It decides interleavings, not correctness - a scenario that
/// only fails under one seed is still a failure.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A pause short enough to change an interleaving and not a schedule.
    async fn jitter(&mut self) {
        match self.next() % 4 {
            0 => tokio::task::yield_now().await,
            1 => tokio::time::sleep(Duration::from_micros(self.next() % 200)).await,
            _ => {}
        }
    }
}

/// A deadline, shared by every task in a scenario.
#[derive(Clone)]
struct Deadline {
    until: Instant,
    stop: Arc<AtomicBool>,
}

impl Deadline {
    fn new() -> Self {
        Deadline {
            until: Instant::now() + budget(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn live(&self) -> bool {
        !self.stop.load(Ordering::Relaxed) && Instant::now() < self.until
    }

    /// Stop everything now - what a writer calls when its own loop is done, so
    /// readers do not keep running against a store nobody is changing.
    fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- harness --

/// The same wiring `summ serve` builds, on a real store.
///
/// Shared behind an `Arc` by every task in a scenario, so all of them go
/// through one `Backend` and one `PullCounters` exactly as concurrent requests
/// to one process do.
struct Harness {
    app: Router,
    backend: Arc<Backend>,
    counters: Arc<PullCounters>,
}

impl Harness {
    fn open(dir: &Path) -> Arc<Self> {
        let backend = Arc::new(
            Backend::open(dir, Engine::Rocks, RegistryOptions::default()).expect("backend opens"),
        );
        // Counting is on, as it is in `summ serve`, but with no flush task:
        // `flush` is the tick, taken by hand where a test needs one.
        let counters = Arc::new(PullCounters::new());
        let app = router(AppState::with_counters(
            backend.clone(),
            ServerConfig::default(),
            counters.clone(),
        ));
        Arc::new(Harness {
            app,
            backend,
            counters,
        })
    }

    async fn flush(&self) -> usize {
        self.backend.flush_pull_counts(&self.counters).await
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collects");
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        headers: Vec<(&str, String)>,
        body: Body,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        self.send(builder.body(body).expect("valid request")).await
    }

    async fn get(&self, uri: &str) -> Reply {
        self.request(Method::GET, uri, Vec::new(), Body::empty())
            .await
    }

    async fn head(&self, uri: &str) -> Reply {
        self.request(Method::HEAD, uri, Vec::new(), Body::empty())
            .await
    }

    async fn delete(&self, uri: &str) -> Reply {
        self.request(Method::DELETE, uri, Vec::new(), Body::empty())
            .await
    }

    /// A monolithic blob push, which is what every real client sends.
    async fn push_blob(&self, repo: &str, bytes: &[u8]) -> Reply {
        let digest = sha256_hex(bytes);
        let opened = self
            .request(
                Method::POST,
                &format!("/v2/{repo}/blobs/uploads/"),
                Vec::new(),
                Body::empty(),
            )
            .await;
        if opened.status != StatusCode::ACCEPTED {
            return opened;
        }
        let location = opened
            .header(header::LOCATION)
            .expect("Location")
            .to_owned();
        self.request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            Vec::new(),
            Body::from(bytes.to_vec()),
        )
        .await
    }

    async fn push_manifest(&self, repo: &str, reference: &str, body: &[u8]) -> Reply {
        self.request(
            Method::PUT,
            &format!("/v2/{repo}/manifests/{reference}"),
            vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
            Body::from(body.to_vec()),
        )
        .await
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn header(&self, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;
const LAYER: &[u8] = b"the layer bytes, such as they are";

/// A manifest over [`CONFIG`] and [`LAYER`], stamped with `round`.
///
/// The stamp is what makes a race observable: the document, and therefore the
/// digest, is unique per round, so a reader can name *which* push it is looking
/// at rather than only whether the bytes are one of a set. An annotation is the
/// right place for it because it changes nothing else about the manifest - the
/// blobs, the media types and the shape are identical across rounds, so the
/// only thing under test is the tag moving.
fn stamped(round: u64) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}],"annotations":{{"round":"{round}"}}}}"#,
        sha256_hex(CONFIG),
        CONFIG.len(),
        sha256_hex(LAYER),
        LAYER.len(),
    )
    .into_bytes()
}

/// A manifest over `CONFIG` and one caller-supplied layer.
fn over_layer(layer: &[u8], round: u64) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}],"annotations":{{"round":"{round}"}}}}"#,
        sha256_hex(CONFIG),
        CONFIG.len(),
        sha256_hex(layer),
        layer.len(),
    )
    .into_bytes()
}

/// The round stamped into a manifest body, read back off the wire.
fn round_of(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(body).ok()?["annotations"]["round"]
        .as_str()?
        .parse()
        .ok()
}

/// Violations collected by a task, reported together.
///
/// A reader that asserted inline would abort the scenario at its first finding
/// and take the interleaving with it; collecting means one run says everything
/// it saw, and the panic message names the interleaving rather than a line
/// number in a spawned task.
type Findings = Vec<String>;

fn report(name: &str, findings: Findings) {
    if !findings.is_empty() {
        let shown: Vec<&String> = findings.iter().take(20).collect();
        panic!(
            "{name}: {} violation(s), first {}:\n  {}",
            findings.len(),
            shown.len(),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
}

// ------------------------------------------- S1: tag repoint under a storm --

/// A tag repointed under a constant stream of pulls is never seen out of order.
///
/// The writer pushes a fresh manifest to `live` round after round; the readers
/// do nothing but pull that tag. Four things have to hold, and only the last is
/// about concurrency:
///
/// - the pull always succeeds, because the tag exists before the readers start
///   and every repoint is a `Put` over it, never a delete-then-write;
/// - the body is byte-exact something that was pushed, and its `round`
///   annotation identifies which;
/// - `Docker-Content-Digest` is the digest of the body actually returned, not
///   of whatever the tag points at now - the two differ by a repoint, and a
///   header taken from a second lookup would be the bug;
/// - **a reader never goes backwards.** One task's requests are strictly
///   sequential, and a `get` on a single RocksDB instance sees every batch
///   already applied, so round numbers observed by one reader must be
///   non-decreasing. This is the assertion a "the digest is one we pushed"
///   check would pass while a stale read went unnoticed.
///
/// `HEAD` is checked against the same sequence rather than separately, because
/// it walks a *different* key set - `T` then `M`, with no `B` read - so a
/// divergence between the two is exactly the kind of thing only a mixed stream
/// finds.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_tag_repoint_is_never_seen_out_of_order() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());

    assert_eq!(
        h.push_blob("race/repoint", CONFIG).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        h.push_blob("race/repoint", LAYER).await.status,
        StatusCode::CREATED
    );

    // Announced before the push that creates it, so a reader can never observe
    // a digest this map does not know: the map is written before the store is.
    let rounds: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));

    let first = stamped(0);
    rounds.lock().unwrap().insert(sha256_hex(&first), 0);
    assert_eq!(
        h.push_manifest("race/repoint", "live", &first).await.status,
        StatusCode::CREATED
    );

    let deadline = Deadline::new();
    let mut tasks = Vec::new();

    for reader in 0..width() {
        let h = h.clone();
        let rounds = rounds.clone();
        let deadline = deadline.clone();
        let mut rng = Rng(seed() ^ reader as u64);
        tasks.push(tokio::spawn(async move {
            let mut findings = Findings::new();
            let mut pulls = 0u64;
            let mut seen = 0u64;
            while deadline.live() {
                rng.jitter().await;
                // Alternate the two lookup paths so a divergence between them
                // shows up in one reader's own ordering.
                let by_head = rng.next().is_multiple_of(3);
                let reply = if by_head {
                    h.head("/v2/race/repoint/manifests/live").await
                } else {
                    h.get("/v2/race/repoint/manifests/live").await
                };
                pulls += 1;

                if reply.status != StatusCode::OK {
                    findings.push(format!(
                        "reader {reader}: pull {pulls} of a tag that always \
                         exists answered {} (a repoint is a Put, never a \
                         delete-then-write)",
                        reply.status
                    ));
                    continue;
                }

                let Some(claimed) = reply.header("docker-content-digest").map(str::to_owned) else {
                    findings.push(format!("reader {reader}: no Docker-Content-Digest"));
                    continue;
                };

                if !by_head {
                    let actual = sha256_hex(&reply.body);
                    if actual != claimed {
                        findings.push(format!(
                            "reader {reader}: Docker-Content-Digest {claimed} \
                             describes a different document than the body \
                             returned ({actual}) - the header was taken from a \
                             second lookup"
                        ));
                        continue;
                    }
                    match round_of(&reply.body) {
                        Some(r) if rounds.lock().unwrap().get(&claimed) == Some(&r) => {}
                        other => {
                            findings.push(format!(
                                "reader {reader}: body stamped {other:?} does \
                                 not match the digest {claimed} it came back \
                                 under"
                            ));
                            continue;
                        }
                    }
                }

                let Some(round) = rounds.lock().unwrap().get(&claimed).copied() else {
                    findings.push(format!(
                        "reader {reader}: tag resolved to {claimed}, which was \
                         never pushed"
                    ));
                    continue;
                };
                if round < seen {
                    findings.push(format!(
                        "reader {reader}: went backwards - saw round {seen}, \
                         then round {round} on a later request"
                    ));
                }
                seen = seen.max(round);
            }
            (findings, pulls)
        }));
    }

    let writer = {
        let h = h.clone();
        let rounds = rounds.clone();
        let deadline = deadline.clone();
        let mut rng = Rng(seed() ^ 0xA5A5);
        tokio::spawn(async move {
            let mut round = 0u64;
            while deadline.live() {
                round += 1;
                let body = stamped(round);
                rounds.lock().unwrap().insert(sha256_hex(&body), round);
                let reply = h.push_manifest("race/repoint", "live", &body).await;
                assert_eq!(
                    reply.status,
                    StatusCode::CREATED,
                    "repointing the tag in round {round}"
                );
                rng.jitter().await;
            }
            deadline.halt();
            round
        })
    };

    let last = writer.await.expect("the writer task");
    let mut findings = Findings::new();
    let mut pulls = 0u64;
    for task in tasks {
        let (mut f, p) = task.await.expect("a reader task");
        findings.append(&mut f);
        pulls += p;
    }
    report("a_tag_repoint_is_never_seen_out_of_order", findings);

    // And the storm left the tag where the last write put it.
    let settled = h.get("/v2/race/repoint/manifests/live").await;
    assert_eq!(settled.status, StatusCode::OK);
    assert_eq!(
        round_of(&settled.body),
        Some(last),
        "the tag must settle on the last round pushed"
    );
    eprintln!("S1: {last} repoints, {pulls} pulls");
    assert!(last > 1 && pulls > 1, "the scenario has to actually race");
}

// ----------------------------------------- S2: blob visibility under push --

/// A blob being pushed is either absent or complete, never partial.
///
/// The ordering rule says the bytes land and are fsynced before the metadata
/// batch that makes them servable commits, so `blob_is_servable` is the switch
/// and it has no intermediate position. What a reader hammering the digest of a
/// blob that is mid-upload must therefore see is a `404` or the whole thing -
/// and if it sees the whole thing, `sha256` of what came back has to be the
/// digest it asked for, because a short body under a content-addressed name is
/// the failure this rule exists to prevent.
///
/// The writer checks the other half from its own side: the instant a manifest
/// `PUT` answers `201`, every layer it names must be pullable. A manifest
/// visible before its blobs is metadata pointing at content that is not there,
/// which is the direction the rule forbids.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_blob_is_never_visible_as_a_partial_body() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());
    assert_eq!(
        h.push_blob("race/blobs", CONFIG).await.status,
        StatusCode::CREATED
    );

    // The blob the readers are chasing: announced before its upload opens, so
    // every reader is racing an upload rather than reading a settled store.
    let inflight: Arc<Mutex<Option<(String, usize)>>> = Arc::new(Mutex::new(None));
    let deadline = Deadline::new();
    let mut tasks = Vec::new();

    for reader in 0..width() {
        let h = h.clone();
        let inflight = inflight.clone();
        let deadline = deadline.clone();
        let mut rng = Rng(seed() ^ (0xB10B + reader as u64));
        tasks.push(tokio::spawn(async move {
            let mut findings = Findings::new();
            let mut hits = 0u64;
            while deadline.live() {
                rng.jitter().await;
                let Some((digest, len)) = inflight.lock().unwrap().clone() else {
                    continue;
                };
                let uri = format!("/v2/race/blobs/blobs/{digest}");
                // A full read and containerd's open-ended range, alternating:
                // they take different paths through `get_blob`, and only one of
                // them consults the file's own length.
                let ranged = rng.next().is_multiple_of(2);
                let reply = if ranged {
                    h.request(
                        Method::GET,
                        &uri,
                        vec![(header::RANGE.as_str(), "bytes=0-".to_owned())],
                        Body::empty(),
                    )
                    .await
                } else {
                    h.get(&uri).await
                };

                match reply.status {
                    StatusCode::NOT_FOUND => {}
                    StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                        hits += 1;
                        if reply.body.len() != len {
                            findings.push(format!(
                                "reader {reader}: {digest} came back as {} of \
                                 {len} bytes (ranged={ranged}) - a blob is \
                                 servable only after its bytes are fsynced",
                                reply.body.len()
                            ));
                        } else if sha256_hex(&reply.body) != digest {
                            findings.push(format!(
                                "reader {reader}: {digest} came back as \
                                 content hashing to {} (ranged={ranged})",
                                sha256_hex(&reply.body)
                            ));
                        }
                    }
                    other => findings.push(format!(
                        "reader {reader}: {digest} answered {other} \
                         (ranged={ranged}); only 404 and a complete body are \
                         legal while it is being pushed"
                    )),
                }
            }
            (findings, hits)
        }));
    }

    let writer = {
        let h = h.clone();
        let inflight = inflight.clone();
        let deadline = deadline.clone();
        tokio::spawn(async move {
            let mut findings = Findings::new();
            let mut round = 0u64;
            while deadline.live() {
                round += 1;
                // A layer big enough that the write is not instantaneous, and
                // unique per round so each one is genuinely a fresh upload.
                let layer: Vec<u8> = format!("layer {round} ")
                    .bytes()
                    .cycle()
                    .take(256 * 1024)
                    .collect();
                let digest = sha256_hex(&layer);
                *inflight.lock().unwrap() = Some((digest.clone(), layer.len()));

                let pushed = h.push_blob("race/blobs", &layer).await;
                assert_eq!(pushed.status, StatusCode::CREATED, "round {round}");

                let body = over_layer(&layer, round);
                let put = h
                    .push_manifest("race/blobs", &format!("r{round}"), &body)
                    .await;
                assert_eq!(put.status, StatusCode::CREATED, "manifest {round}");

                // The commit point has passed, so every layer it names is on
                // disk and servable now - not eventually.
                for want in [sha256_hex(CONFIG), digest.clone()] {
                    let reply = h.get(&format!("/v2/race/blobs/blobs/{want}")).await;
                    if reply.status != StatusCode::OK {
                        findings.push(format!(
                            "round {round}: manifest committed but its blob \
                             {want} answered {} - metadata became visible \
                             before its content",
                            reply.status
                        ));
                    }
                }
            }
            deadline.halt();
            (findings, round)
        })
    };

    let (mut findings, rounds) = writer.await.expect("the writer task");
    let mut hits = 0u64;
    for task in tasks {
        let (mut f, hit) = task.await.expect("a reader task");
        findings.append(&mut f);
        hits += hit;
    }
    report("a_blob_is_never_visible_as_a_partial_body", findings);
    eprintln!("S2: {rounds} blob pushes, {hits} reads that found one");
    assert!(rounds > 1, "the scenario has to actually race");
}

// -------------------------------------- S3: push racing a reference delete --

/// A manifest push racing a delete of the blob it names never leaves a 500 or
/// an unreadable manifest behind.
///
/// This one aims at the window the plan records as open: `put_manifest` plans
/// its batch - which is where references are validated - then writes the
/// archive copy and fsyncs it, and only then applies. Validation and commit are
/// therefore separated by milliseconds of disk I/O with no lock across them, so
/// a `DELETE /v2/<name>/blobs/<digest>` landing in between can retract the very
/// edge the plan checked.
///
/// What that window can produce is a manifest committed against a blob that is
/// no longer a member of the repository - which is *also* what an ordinary
/// out-of-order client produces, because the spec's blob delete does not
/// consult `R`. So "the manifest names a missing blob" cannot be the assertion:
/// it is reachable with no race at all. What the test asserts instead is that
/// neither operation can be made to fail in a way that is nobody's contract -
/// no `500` from either side, and a manifest that answered `201` is still
/// readable byte-exact afterwards, never the `M`-without-`B` corruption path.
///
/// The width of the window is reported rather than asserted. It is a real
/// number about a known-open item, and a number that varies with the machine is
/// not something to fail a release on.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_push_racing_a_blob_delete_never_corrupts_the_store() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());
    assert_eq!(
        h.push_blob("race/validate", CONFIG).await.status,
        StatusCode::CREATED
    );

    // How wide the window actually is, measured rather than guessed. One
    // uncontended push, timed: the delete's delay is then spread over twice
    // that, so a run sweeps the whole of it - before the plan, between the plan
    // and the apply, and after the batch has landed - on whatever machine it is
    // running on. A fixed spread tuned on one laptop degenerates into probing
    // one side of the window everywhere else.
    let probe = {
        let layer = b"the timing probe".to_vec();
        assert_eq!(
            h.push_blob("race/validate", &layer).await.status,
            StatusCode::CREATED
        );
        let body = over_layer(&layer, 0);
        let started = Instant::now();
        assert_eq!(
            h.push_manifest("race/validate", "probe", &body)
                .await
                .status,
            StatusCode::CREATED
        );
        started.elapsed()
    };
    let spread = (probe.as_micros() as u64 * 2).max(50);

    let deadline = Deadline::new();
    // The outcome classes, tallied rather than asserted. Which of them a run
    // lands in depends on where the delete fell relative to the push's plan and
    // its apply, so the tally is what says whether a run probed the window at
    // all - a scenario reporting only zeroes has usually just never raced.
    let mut refused = 0u64; // the delete won outright: 400 MANIFEST_BLOB_UNKNOWN
    let mut layer_survived = 0u64; // the delete planned before the `R` edge existed
    let mut committed_without_its_layer = 0u64; // the delete landed after the commit
    let mut findings = Findings::new();
    let mut rounds = 0u64;
    let mut rng = Rng(seed() ^ 0xDE1E);

    while deadline.live() {
        rounds += 1;
        let layer: Vec<u8> = format!("racy layer {rounds}").into_bytes();
        let digest = sha256_hex(&layer);
        assert_eq!(
            h.push_blob("race/validate", &layer).await.status,
            StatusCode::CREATED
        );

        let body = over_layer(&layer, rounds);
        let manifest_digest = sha256_hex(&body);
        let tag = format!("r{rounds}");

        // Issued together: the delete is aiming at the gap between the plan
        // that validates the layer and the batch that commits the manifest.
        let put = {
            let h = h.clone();
            let body = body.clone();
            let tag = tag.clone();
            tokio::spawn(async move { h.push_manifest("race/validate", &tag, &body).await })
        };
        let del = {
            let h = h.clone();
            let digest = digest.clone();
            let mut rng = Rng(rng.next());
            tokio::spawn(async move {
                let delay = rng.next() % spread;
                if delay > 0 {
                    tokio::time::sleep(Duration::from_micros(delay)).await;
                }
                h.delete(&format!("/v2/race/validate/blobs/{digest}")).await
            })
        };
        let (put, del) = (
            put.await.expect("the push task"),
            del.await.expect("the delete task"),
        );

        match put.status {
            // Committed, or refused because the layer had already gone. Both
            // are honest answers to a client doing two contradictory things.
            StatusCode::CREATED => {}
            StatusCode::BAD_REQUEST => refused += 1,
            other => findings.push(format!(
                "round {rounds}: manifest PUT answered {other}; a push racing a \
                 blob delete is a client's problem, never the registry's"
            )),
        }
        match del.status {
            StatusCode::ACCEPTED | StatusCode::NOT_FOUND => {}
            other => findings.push(format!("round {rounds}: blob DELETE answered {other}")),
        }

        if put.status == StatusCode::CREATED {
            // Whatever the interleaving decided, the manifest it committed has
            // to read back - `M` and `B` are written in one batch, so a miss
            // here is a torn read and a 500 is the corruption path.
            let read = h
                .get(&format!("/v2/race/validate/manifests/{manifest_digest}"))
                .await;
            if read.status != StatusCode::OK {
                findings.push(format!(
                    "round {rounds}: manifest committed as {manifest_digest} \
                     reads back {}",
                    read.status
                ));
            } else if read.body.as_ref() != body.as_slice() {
                findings.push(format!(
                    "round {rounds}: manifest {manifest_digest} did not come \
                     back byte-exact"
                ));
            }
            if del.status == StatusCode::ACCEPTED {
                let layer = h.get(&format!("/v2/race/validate/blobs/{digest}")).await;
                if layer.status == StatusCode::NOT_FOUND {
                    committed_without_its_layer += 1;
                } else {
                    // The delete drained `R` before the push wrote its edge, so
                    // it took `P` and left the blob servable through the edge
                    // that arrived after it. Not a fault: the delete answered
                    // for the state it saw.
                    layer_survived += 1;
                }
            }
        }
    }

    report(
        "a_push_racing_a_blob_delete_never_corrupts_the_store",
        findings,
    );
    eprintln!(
        "S3: {rounds} races over a {probe:?} push - {refused} refused the \
         push outright, \
         {layer_survived} committed with the layer still servable, \
         {committed_without_its_layer} committed with the layer gone (the \
         plan/apply window - an open item, reported and not asserted)"
    );
    assert!(rounds > 1, "the scenario has to actually race");
}

// ------------------------------------- S4: reads racing a repository delete --

/// Pulls racing a repository delete and its sweep answer, or 404, and never
/// 500 - and a repository that has gone does not come back.
///
/// Two windows are in range here, and they are worth naming because they are
/// the reason this is a scenario rather than a unit test.
///
/// The first is inside one read. `get_manifest_by_tag` is `T`, then `M`, then
/// `B`, three independent lookups; the sweep drops all three ranges in one
/// batch, but a reader can be *between* two of its own lookups when that batch
/// lands. Holding an `M` whose `B` has gone is the one state `stored_manifest`
/// calls corruption rather than a miss, and corruption is a `500`. If that
/// window is reachable, this is the test that finds it.
///
/// The second is the interner. The name is released in the delete's batch and
/// the LRU in front of it is evicted *after* the apply, so a lookup in between
/// resolves a name the catalog has already stopped listing. That is harmless by
/// construction - the id is never reused - but "harmless" is a claim, and the
/// monotonic check below is what tests it: once a reader has been told the
/// repository is gone, nothing may tell it otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reads_racing_a_repository_delete_never_5xx() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());

    let deadline = Deadline::new();
    let mut tasks = Vec::new();

    // Readers chase whichever generation the writer is on. A generation is a
    // repository name of its own, so "gone" is permanent for that name and a
    // later `200` is unambiguously a resurrection rather than the next round.
    let generation = Arc::new(AtomicU64::new(0));

    for reader in 0..width() {
        let h = h.clone();
        let generation = generation.clone();
        let deadline = deadline.clone();
        let mut rng = Rng(seed() ^ (0x5EEB + reader as u64));
        tasks.push(tokio::spawn(async move {
            let mut findings = Findings::new();
            let mut vanished: Option<u64> = None;
            let mut reads = 0u64;
            while deadline.live() {
                rng.jitter().await;
                let gen = generation.load(Ordering::Relaxed);
                if gen == 0 {
                    continue;
                }
                let repo = format!("race/gone{gen}");
                let uri = match rng.next() % 3 {
                    0 => format!("/v2/{repo}/manifests/live"),
                    1 => format!("/v2/{repo}/tags/list"),
                    _ => format!("/api/v1/manifests/{repo}@live"),
                };
                let reply = h.get(&uri).await;
                reads += 1;

                if reply.status.is_server_error() {
                    findings.push(format!(
                        "reader {reader}: {uri} answered {} - a read landing \
                         inside the sweep is a miss, never corruption",
                        reply.status
                    ));
                    continue;
                }
                if !matches!(reply.status, StatusCode::OK | StatusCode::NOT_FOUND) {
                    findings.push(format!("reader {reader}: {uri} answered {}", reply.status));
                    continue;
                }
                match (reply.status, vanished) {
                    (StatusCode::NOT_FOUND, None) => vanished = Some(gen),
                    (StatusCode::OK, Some(g)) if g == gen => findings.push(format!(
                        "reader {reader}: {uri} answered 200 after generation \
                         {g} had already answered 404 - a deleted repository \
                         came back"
                    )),
                    // A new generation resets the observation.
                    (_, Some(g)) if g != gen => vanished = None,
                    _ => {}
                }
            }
            (findings, reads)
        }));
    }

    let writer = {
        let h = h.clone();
        let generation = generation.clone();
        let deadline = deadline.clone();
        tokio::spawn(async move {
            let mut findings = Findings::new();
            let mut gen = 0u64;
            while deadline.live() {
                gen += 1;
                let repo = format!("race/gone{gen}");
                assert_eq!(h.push_blob(&repo, CONFIG).await.status, StatusCode::CREATED);
                assert_eq!(h.push_blob(&repo, LAYER).await.status, StatusCode::CREATED);
                assert_eq!(
                    h.push_manifest(&repo, "live", &stamped(gen)).await.status,
                    StatusCode::CREATED
                );
                generation.store(gen, Ordering::Relaxed);

                let deleted = h.delete(&format!("/api/v1/repositories/{repo}")).await;
                if deleted.status != StatusCode::ACCEPTED {
                    findings.push(format!(
                        "generation {gen}: DELETE answered {}",
                        deleted.status
                    ));
                }
                // The sweep, run here rather than by the background task, so
                // it is guaranteed to overlap the readers rather than to
                // happen to.
                if let Err(e) = h.backend.sweep_dead_repos().await {
                    findings.push(format!("generation {gen}: sweep failed: {e}"));
                }
            }
            deadline.halt();
            (findings, gen)
        })
    };

    let (mut findings, generations) = writer.await.expect("the writer task");
    let mut reads = 0u64;
    for task in tasks {
        let (mut f, r) = task.await.expect("a reader task");
        findings.append(&mut f);
        reads += r;
    }
    report("reads_racing_a_repository_delete_never_5xx", findings);

    // Nothing outlived the sweep: every generation's name is out of the
    // catalog and there is no work left behind.
    let catalog = h.get("/v2/_catalog").await.json();
    assert_eq!(
        catalog["repositories"].as_array().map(Vec::len),
        Some(0),
        "every deleted name must be out of the catalog"
    );
    assert_eq!(
        h.backend.sweep_dead_repos().await.expect("a final sweep"),
        0,
        "a finished sweep leaves no work behind"
    );
    eprintln!("S4: {generations} delete cycles, {reads} racing reads");
    assert!(generations > 1 && reads > 1, "the scenario has to race");
}

// --------------------------------------------- S5: writers racing one tag --

/// Concurrent repoints of one tag leave exactly one manifest wearing it.
///
/// `stage_set_tag` reads the tag's current digest so it can retract the
/// displaced `G` edge, and that read is in the *plan* while the retraction is
/// in the *apply*. Two writers repointing at once therefore both see the same
/// predecessor and both retract only it: the loser's own edge is never dropped
/// by the winner, because the winner never saw it. If that is what happens, the
/// symptom is not a wrong tag lookup - `T` is a single key and last write wins
/// cleanly - but a discovery API that reports the tag on two manifests at once,
/// which is what the UI would draw.
///
/// So the assertion is a cross-check between the two sides of the same fact:
/// `T` says the tag points at one digest, and `G` - which is what
/// `/api/v1/manifests/<name>@<digest>` reads to list a manifest's tags - must
/// agree with it and with nothing else.
///
/// Tag history is the second check. Every push writes a `Created` event
/// unconditionally, each writer's manifest is unique per round so no two events
/// can collide on a key, and nothing here deletes anything - so the count is
/// exact, and a shortfall is a lost write.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writers_racing_one_tag_leave_exactly_one_tag_edge() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());
    assert_eq!(
        h.push_blob("race/contend", CONFIG).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        h.push_blob("race/contend", LAYER).await.status,
        StatusCode::CREATED
    );

    // The sequential case first, so a failure below is unambiguously about
    // concurrency: pushed one after another, a repoint retracts the displaced
    // edge and the loser stops reporting the tag.
    let first = stamped(0);
    let second = stamped(1);
    assert_eq!(
        h.push_manifest("race/contend", "warmup", &first)
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        h.push_manifest("race/contend", "warmup", &second)
            .await
            .status,
        StatusCode::CREATED
    );
    let displaced = h
        .get(&format!(
            "/api/v1/manifests/race/contend@{}",
            sha256_hex(&first)
        ))
        .await
        .json();
    assert_eq!(
        displaced["tags"].as_array().map(Vec::len),
        Some(0),
        "a sequential repoint retracts the `G` edge it displaced"
    );

    let deadline = Deadline::new();
    let writers = width().max(2);
    let mut tasks = Vec::new();

    for writer in 0..writers {
        let h = h.clone();
        let deadline = deadline.clone();
        let mut rng = Rng(seed() ^ (0xC0FF + writer as u64));
        tasks.push(tokio::spawn(async move {
            let mut pushed = Vec::new();
            let mut round = 0u64;
            while deadline.live() {
                round += 1;
                // Unique across writers and rounds, so every push is its own
                // digest and every history event its own key.
                let body = stamped((writer as u64 + 1) * 1_000_000 + round);
                let reply = h.push_manifest("race/contend", "live", &body).await;
                assert_eq!(reply.status, StatusCode::CREATED, "writer {writer}");
                pushed.push(sha256_hex(&body));
                rng.jitter().await;
            }
            pushed
        }));
    }

    // The first writer to finish stops the rest, so the run ends at a point
    // nobody is mid-push.
    let mut all = Vec::new();
    let mut first = true;
    for task in tasks {
        let pushed = task.await.expect("a writer task");
        if first {
            deadline.halt();
            first = false;
        }
        all.extend(pushed);
    }

    let resolved = h.get("/v2/race/contend/manifests/live").await;
    assert_eq!(resolved.status, StatusCode::OK);
    let winner = resolved
        .header("docker-content-digest")
        .expect("a digest")
        .to_owned();
    assert!(
        all.contains(&winner),
        "the tag settled on {winner}, which nobody pushed"
    );

    // `G` must name the winner and only the winner. Checking every digest
    // pushed is the point: a stale edge is invisible from the tag's side.
    let mut findings = Findings::new();
    let mut distinct: Vec<&String> = all.iter().collect();
    distinct.sort();
    distinct.dedup();
    for digest in &distinct {
        let info = h
            .get(&format!("/api/v1/manifests/race/contend@{digest}"))
            .await;
        assert_eq!(info.status, StatusCode::OK, "{digest} must still exist");
        let json = info.json();
        let tags: Vec<&str> = json["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .filter_map(|t| t.as_str())
            .collect();
        let wears_it = tags.contains(&"live");
        if wears_it != (*digest == &winner) {
            findings.push(format!(
                "manifest {digest} reports tags {tags:?} while `live` resolves \
                 to {winner} - a `G` edge disagrees with `T`, so the tag \
                 appears on {} manifests",
                if wears_it { "more than one" } else { "no" }
            ));
        }
    }
    report(
        "writers_racing_one_tag_leave_exactly_one_tag_edge",
        findings,
    );

    // Every push is an event, and no two of them can share a key. Counted by
    // following the cursor rather than by asking for them all at once: `?n=` is
    // clamped to the page ceiling, so a single request would silently answer
    // with a page instead of a total.
    let mut events = 0usize;
    let mut instants: Vec<u64> = Vec::new();
    let mut uri = "/api/v1/tag-history/race/contend@live".to_string();
    loop {
        let page = h.get(&uri).await;
        assert_eq!(page.status, StatusCode::OK);
        let page = page.json();
        let rows = page["events"].as_array().expect("events");
        events += rows.len();
        instants.extend(rows.iter().map(|e| e["at"].as_u64().expect("at")));
        let Some(next) = page["next"].as_object() else {
            break;
        };
        uri = format!(
            "/api/v1/tag-history/race/contend@live?before={}&last={}",
            next["before"].as_u64().expect("before"),
            next["last"].as_str().expect("last"),
        );
    }
    assert_eq!(
        events,
        all.len(),
        "tag history records pushes, not changes: {} concurrent pushes must be \
         {} events, and events sharing an instant must not be lost to the \
         cursor",
        all.len(),
        all.len()
    );
    let mut descending = instants.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        instants, descending,
        "pages stay newest-first across the cursor even where writers shared \
         an instant"
    );
    eprintln!(
        "S5: {writers} writers, {} pushes, {} distinct manifests",
        all.len(),
        distinct.len()
    );
    assert!(all.len() > writers, "the scenario has to actually race");
}

// ------------------------------------------- S6: parallel first pushes --

/// Blobs pushed in parallel into a cold store all succeed.
///
/// This is the regression test for the fan-out directory race. Every commit
/// into a fresh store finds `blobs/sha256` missing and tries to create it, and
/// `oras push` uploads an artifact's blobs concurrently, so two writers losing
/// that race is what an ordinary first push does rather than a rare
/// interleaving. Taking `AlreadyExists` out to the caller turned it into a
/// `500` with the layer already uploaded.
///
/// Three shapes at once, because they fail differently: distinct blobs into one
/// repository (the `oras push` case, contending on the fan-out levels), the
/// *same* blob into many repositories (contending on the final rename as well),
/// and the same blob into the same repository twice (two upload sessions
/// committing identical content to one path).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_first_pushes_into_a_cold_store_all_succeed() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());

    let shared = b"the layer every image shares".to_vec();
    let mut tasks = Vec::new();

    for i in 0..width().max(4) {
        // Distinct content, one repository.
        {
            let h = h.clone();
            let body = format!("distinct layer {i}").into_bytes();
            tasks.push(tokio::spawn(async move {
                (
                    format!("distinct {i}"),
                    h.push_blob("cold/one", &body).await.status,
                )
            }));
        }
        // One blob, many repositories.
        {
            let h = h.clone();
            let body = shared.clone();
            tasks.push(tokio::spawn(async move {
                (
                    format!("shared into cold/many{i}"),
                    h.push_blob(&format!("cold/many{i}"), &body).await.status,
                )
            }));
        }
        // One blob, one repository, twice over.
        {
            let h = h.clone();
            let body = shared.clone();
            tasks.push(tokio::spawn(async move {
                (
                    format!("shared into cold/same ({i})"),
                    h.push_blob("cold/same", &body).await.status,
                )
            }));
        }
    }

    let mut findings = Findings::new();
    for task in tasks {
        let (what, status) = task.await.expect("a push task");
        if status != StatusCode::CREATED {
            findings.push(format!(
                "{what} answered {status}; losing the race to create a fan-out \
                 level is not an error"
            ));
        }
    }
    report(
        "parallel_first_pushes_into_a_cold_store_all_succeed",
        findings,
    );

    // And the content is there once, correct, under every name that claimed it.
    let digest = sha256_hex(&shared);
    for repo in ["cold/same", "cold/many0"] {
        let reply = h.get(&format!("/v2/{repo}/blobs/{digest}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{repo}");
        assert_eq!(reply.body.as_ref(), shared.as_slice(), "{repo}");
    }
}

// ---------------------------------------------- S7: counting under load --

/// Concurrent pulls are counted exactly once each.
///
/// The accumulator is the one place in summ where a request mutates shared
/// state in memory, and the fold that turns it into a `WriteBatch` is the one
/// read-modify-write in the system. Both are supposed to be exact between
/// flushes - "approximate" is about crashes and saturation, not about
/// arithmetic - so N concurrent pulls must be N, not N minus a few lost
/// increments.
///
/// The `HEAD`s in the stream are the other half. containerd issues `HEAD` then
/// `GET` on every cold pull, so a `HEAD` that counted would double every number
/// in the product; running them concurrently with the `GET`s is what shows the
/// filter is on the request and not on a race-free ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_pulls_are_counted_exactly_once() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(dir.path());
    assert_eq!(
        h.push_blob("race/counted", CONFIG).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        h.push_blob("race/counted", LAYER).await.status,
        StatusCode::CREATED
    );
    let body = stamped(1);
    assert_eq!(
        h.push_manifest("race/counted", "live", &body).await.status,
        StatusCode::CREATED
    );
    // The push itself counts nothing; flush so the window starts at zero.
    h.flush().await;

    let per_task = 40u64;
    let tasks_n = width().max(4) as u64;
    let mut tasks = Vec::new();
    for task in 0..tasks_n {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng(seed() ^ (0xC0DE + task));
            for _ in 0..per_task {
                // A HEAD before the GET, as containerd does, plus a blob read.
                h.head("/v2/race/counted/manifests/live").await;
                let pulled = h.get("/v2/race/counted/manifests/live").await;
                assert_eq!(pulled.status, StatusCode::OK);
                let blob = h
                    .get(&format!("/v2/race/counted/blobs/{}", sha256_hex(LAYER)))
                    .await;
                assert_eq!(blob.status, StatusCode::OK);
                rng.jitter().await;
            }
        }));
    }
    for task in tasks {
        task.await.expect("a pulling task");
    }

    // One flush, after everything has been counted, so what is asserted is the
    // accumulator's arithmetic rather than the flush interval's timing.
    h.flush().await;

    let expected = tasks_n * per_task;
    let repo = h.get("/api/v1/pull-counts/race/counted").await.json();
    assert_eq!(
        repo["totals"]["manifest_pulls"].as_u64(),
        Some(expected),
        "{expected} concurrent GETs, and the HEAD beside each one counts for \
         nothing"
    );
    assert_eq!(
        repo["totals"]["blob_pulls"].as_u64(),
        Some(expected),
        "one blob read per pull"
    );
    assert_eq!(
        repo["totals"]["bytes_out"].as_u64(),
        Some(expected * LAYER.len() as u64),
        "bytes are metered on the body, so every one of them is counted once"
    );

    // The tag and the manifest are their own series, and both saw the same
    // pulls: the client asked by tag, so all three scopes take it.
    let tag = h.get("/api/v1/pull-counts/race/counted@live").await.json();
    assert_eq!(tag["totals"]["manifest_pulls"].as_u64(), Some(expected));
    let by_digest = h
        .get(&format!(
            "/api/v1/pull-counts/race/counted@{}",
            sha256_hex(&body)
        ))
        .await
        .json();
    assert_eq!(
        by_digest["totals"]["manifest_pulls"].as_u64(),
        Some(expected)
    );
    eprintln!("S7: {expected} pulls counted exactly");
}
