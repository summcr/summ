//! Purge over the real stack: the part no in-memory registry can check, which
//! is whether the bytes actually left the disk.
//!
//! Every test here runs a pass by hand rather than waiting for a tick, and
//! most run two: a blob is marked on the pass that first finds it unreferenced
//! and reclaimed on a later one, so "purge, and then look" would pass while
//! reclaiming nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use sha2::{Digest as _, Sha256};
use summ_registry::RegistryOptions;
use summ_server::backend::{Backend, Engine};
use summ_server::config::{PurgeConfig, ServerConfig};
use summ_server::{router, AppState};
use tower::ServiceExt;

// ---------------------------------------------------------------- harness --

struct Harness {
    app: Router,
    backend: Arc<Backend>,
    dir: PathBuf,
}

impl Harness {
    /// Zero grace and zero TTL, so a test runs the clocks rather than waiting
    /// them out. Nothing else changes: the pass is the one `summ serve` runs.
    fn eager(dir: &Path) -> Self {
        Self::with_purge(
            dir,
            PurgeConfig {
                grace: Duration::ZERO,
                upload_ttl: Duration::ZERO,
                ..PurgeConfig::default()
            },
        )
    }

    fn with_purge(dir: &Path, purge: PurgeConfig) -> Self {
        let backend = Arc::new(
            Backend::open(dir, Engine::Rocks, RegistryOptions::default())
                .expect("backend opens")
                .with_purge(purge),
        );
        let app = router(AppState::new(backend.clone(), ServerConfig::default()));
        Harness {
            app,
            backend,
            dir: dir.to_path_buf(),
        }
    }

    async fn send(&self, method: Method, uri: &str, body: Body) -> (StatusCode, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .expect("valid request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collects");
        (status, body.to_vec())
    }

    async fn get(&self, uri: &str) -> (StatusCode, Vec<u8>) {
        self.send(Method::GET, uri, Body::empty()).await
    }

    async fn json(&self, method: Method, uri: &str) -> serde_json::Value {
        let (status, body) = self.send(method, uri, Body::empty()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        serde_json::from_slice(&body).expect("JSON body")
    }

    /// Push a blob and return its digest, as `docker push` would.
    async fn push_blob(&self, repo: &str, bytes: &[u8]) -> String {
        let digest = sha256_hex(bytes);
        let location = self.open_upload(repo).await;
        let (status, _) = self
            .send(
                Method::PUT,
                &format!("{location}?digest={digest}"),
                Body::from(bytes.to_vec()),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        digest
    }

    /// Open a session and leave it open.
    async fn open_upload(&self, repo: &str) -> String {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v2/{repo}/blobs/uploads/"))
            .body(Body::empty())
            .expect("valid request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        response
            .headers()
            .get(header::LOCATION)
            .expect("Location")
            .to_str()
            .expect("ASCII")
            .to_owned()
    }

    /// Push one image: a config blob, a layer, and a manifest over both.
    async fn push_image(&self, repo: &str, tag: &str, layer: &[u8]) -> String {
        let config = self.push_blob(repo, b"{}").await;
        let layer_digest = self.push_blob(repo, layer).await;
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config,
                "size": 2,
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer.len(),
            }],
        });
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v2/{repo}/manifests/{tag}"))
            .header(
                header::CONTENT_TYPE,
                "application/vnd.oci.image.manifest.v1+json",
            )
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .expect("valid request");
        let response = self.app.clone().oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::CREATED, "pushing a manifest");
        layer_digest
    }

    /// Push a manifest under its own digest and never tag it - the shape a
    /// digest-pinned deployment leaves behind.
    async fn push_untagged(&self, repo: &str) -> String {
        let config = self.push_blob(repo, b"{}").await;
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config,
                "size": 2,
            },
            "layers": [],
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let digest = sha256_hex(&raw);
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v2/{repo}/manifests/{digest}"))
            .header(
                header::CONTENT_TYPE,
                "application/vnd.oci.image.manifest.v1+json",
            )
            .body(Body::from(raw))
            .expect("valid request");
        let response = self.app.clone().oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::CREATED);
        digest
    }

    /// Where a digest's bytes live. The layout is a pure function of the
    /// digest, which is what lets a test look without asking the registry.
    fn blob_path(&self, digest: &str) -> PathBuf {
        let hex = digest.split_once(':').expect("algo:hex").1;
        self.dir
            .join("blobs")
            .join("sha256")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex[4..6])
            .join(hex)
    }

    fn staging_dir(&self) -> PathBuf {
        self.dir.join("uploads")
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

// ------------------------------------------------------------------ tests --

/// The headline: a repository is deleted, and the disk gives the space back.
#[tokio::test]
async fn deleting_a_repository_eventually_gives_the_bytes_back() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());
    let layer = summ.push_image("demo/app", "v1", b"a layer of bytes").await;
    let path = summ.blob_path(&layer);
    assert!(path.exists(), "the push wrote the bytes");

    let (status, _) = summ
        .send(
            Method::DELETE,
            "/api/v1/repositories/demo/app",
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    summ.backend.sweep_dead_repos().await.expect("sweep");
    assert!(
        path.exists(),
        "a delete is a metadata operation - the bytes are purge's business"
    );

    // First pass marks, second collects. That is the design and not a quirk of
    // the fixture: the mark is what a mount would retract.
    let marking = summ.backend.purge_once(false).await.expect("pass");
    assert!(marking.marked >= 2, "config and layer: {marking:?}");
    assert_eq!(marking.blobs, 0, "nothing is taken on the pass that marks");
    assert!(path.exists());

    let collecting = summ.backend.purge_once(false).await.expect("pass");
    assert_eq!(collecting.blobs, 2, "config and layer");
    assert_eq!(collecting.bytes, 16 + 2);
    assert!(!path.exists(), "the bytes are gone");
}

/// Nothing a live manifest names is ever taken, however many passes run.
#[tokio::test]
async fn a_live_image_survives_every_pass() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());
    let layer = summ.push_image("demo/app", "v1", b"still wanted").await;

    for _ in 0..3 {
        let report = summ.backend.purge_once(false).await.expect("pass");
        assert_eq!((report.blobs, report.marked), (0, 0), "{report:?}");
    }

    let (status, body) = summ.get(&format!("/v2/demo/app/blobs/{layer}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"still wanted", "and the bytes are the ones pushed");
    assert!(summ.blob_path(&layer).exists());
}

/// An untagged manifest is left alone by default and taken when asked for.
///
/// Two registries on one directory, opened one after the other, because the
/// switch is a startup decision and RocksDB holds the store to one process.
#[tokio::test]
async fn untagged_manifests_go_only_when_the_operator_asks() {
    let dir = tempfile::tempdir().unwrap();
    let digest = {
        let summ = Harness::eager(dir.path());
        let digest = summ.push_untagged("demo/app").await;
        // The clocks are wound right down and it still stands, because the
        // switch is off: pulling by digest is ordinary, and a manifest nobody
        // tagged is not by itself garbage.
        let report = summ.backend.purge_once(false).await.expect("pass");
        assert_eq!(report.manifests, 0, "{report:?}");
        let (status, _) = summ.get(&format!("/v2/demo/app/manifests/{digest}")).await;
        assert_eq!(status, StatusCode::OK);
        digest
    };

    let summ = Harness::with_purge(
        dir.path(),
        PurgeConfig {
            grace: Duration::ZERO,
            upload_ttl: Duration::ZERO,
            untagged: true,
            untagged_min_age: Duration::ZERO,
            ..PurgeConfig::default()
        },
    );
    let report = summ.backend.purge_once(false).await.expect("pass");
    assert_eq!(report.manifests, 1, "{report:?}");
    let (status, _) = summ.get(&format!("/v2/demo/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An upload nobody finished: the session goes, and so does its staging file.
#[tokio::test]
async fn an_abandoned_upload_takes_its_staging_file_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());
    let location = summ.open_upload("demo/app").await;

    let staged: Vec<_> = std::fs::read_dir(summ.staging_dir())
        .expect("uploads/")
        .map(|e| e.expect("entry").path())
        .collect();
    assert_eq!(staged.len(), 1, "the session staged a file");

    let report = summ.backend.purge_once(false).await.expect("pass");
    assert_eq!(report.uploads, 1);
    assert!(
        !staged[0].exists(),
        "the staging file went with the session"
    );

    let (status, _) = summ.get(&location).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "and the session is unknown to the registry"
    );
}

/// A name interned by an upload that was cancelled, which would otherwise sit
/// in `_catalog` for ever describing nothing.
#[tokio::test]
async fn an_empty_name_is_retired_on_the_pass_after_the_one_that_saw_it() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());
    let location = summ.open_upload("demo/ghost").await;
    let (status, _) = summ.send(Method::DELETE, &location, Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cancelling the upload");

    let catalog = summ.json(Method::GET, "/v2/_catalog").await;
    assert_eq!(catalog["repositories"], serde_json::json!(["demo/ghost"]));

    // The first pass only learns where the id counter stood, because a name
    // interned during a pass must not be retired by it.
    let first = summ.backend.purge_once(false).await.expect("pass");
    assert_eq!(first.repositories, 0);

    let second = summ.backend.purge_once(false).await.expect("pass");
    assert_eq!(second.repositories, 1, "{second:?}");
    let catalog = summ.json(Method::GET, "/v2/_catalog").await;
    assert_eq!(
        catalog["repositories"],
        serde_json::json!([]),
        "the name is released"
    );
}

/// The API: a dry run counts and writes nothing, and `GET` reports the last
/// pass that did.
#[tokio::test]
async fn the_api_runs_a_pass_and_remembers_the_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());
    let layer = summ.push_image("demo/app", "v1", b"reclaim me").await;
    let (status, _) = summ
        .send(
            Method::DELETE,
            "/api/v1/repositories/demo/app",
            Body::empty(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    summ.backend.sweep_dead_repos().await.expect("sweep");

    let before = summ.json(Method::GET, "/api/v1/purge").await;
    assert_eq!(before["last"], serde_json::Value::Null, "no pass yet");

    let dry = summ.json(Method::POST, "/api/v1/purge?dry-run=true").await;
    assert_eq!(dry["dry_run"], true);
    assert!(dry["marked"].as_u64().unwrap() >= 2, "{dry}");
    assert!(
        summ.blob_path(&layer).exists(),
        "a dry run reclaims nothing"
    );
    let after_dry = summ.json(Method::GET, "/api/v1/purge").await;
    assert_eq!(
        after_dry["last"],
        serde_json::Value::Null,
        "a dry run is not a pass anyone should read as one"
    );

    let marking = summ.json(Method::POST, "/api/v1/purge").await;
    assert_eq!(marking["dry_run"], false);
    let collecting = summ.json(Method::POST, "/api/v1/purge").await;
    assert_eq!(collecting["blobs"], 2, "{collecting}");
    assert_eq!(collecting["bytes"], 12);
    assert!(!summ.blob_path(&layer).exists());

    let last = summ.json(Method::GET, "/api/v1/purge").await;
    assert_eq!(last["last"]["blobs"], 2, "the pass that reclaimed");
    assert!(last["last"]["started_at"].as_u64().unwrap() > 0);
}

/// The endpoint's method table, which is the one place a `POST` exists on this
/// API at all.
#[tokio::test]
async fn the_purge_route_takes_get_head_and_post_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let summ = Harness::eager(dir.path());

    let (status, _) = summ
        .send(Method::HEAD, "/api/v1/purge", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = summ
        .send(Method::DELETE, "/api/v1/purge", Body::empty())
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (status, _) = summ
        .send(Method::POST, "/api/v1/purge/demo/app", Body::empty())
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "purge is registry-wide; there is no per-repository spelling"
    );
}
