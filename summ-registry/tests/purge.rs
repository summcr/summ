//! Purge: what each pass reclaims, and everything it must decline to.

mod common;

use common::*;
use summ_core::{keys, Digest};
use summ_registry::Registry;

/// A blob nothing has ever referenced: uploaded, and its manifest never came.
///
/// Two clocks have to elapse, not one - the membership's and the mark's - and
/// this is the test that says so, because the doubling is the visible cost of
/// having no digest-to-repositories index.
#[test]
fn an_abandoned_upload_loses_its_membership_and_then_its_bytes() {
    let (_dir, reg) = fixture();
    let (digest, size) = upload(&reg, "demo/app", "orphan");
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    // Inside the grace period nothing moves.
    let step = reg
        .sweep_repo_blobs(None, 10, at(500), false)
        .expect("sweep");
    assert_eq!(
        step.retracted, 0,
        "the membership is younger than the cutoff"
    );

    let step = reg
        .sweep_repo_blobs(None, 10, at(2_000), false)
        .expect("sweep");
    assert_eq!(step.retracted, 1);
    assert!(
        !reg.engine()
            .exists_prefix(&keys::repo_blob(repo, &digest))
            .unwrap(),
        "P is gone"
    );

    // The blob is now unreferenced, which earns it a mark and nothing else.
    let scan = reg
        .scan_blobs(None, 10, at(2_000), at(2_000), false)
        .unwrap();
    assert_eq!((scan.marked, scan.ripe.len()), (1, 0), "marked, not taken");
    assert_eq!(
        reg.collect_blob(&digest, at(1_500)).unwrap(),
        None,
        "a mark younger than the grace period has not stood for it"
    );

    // One grace period later it is ripe, and only then does it go.
    let scan = reg
        .scan_blobs(None, 10, at(90_000), at(90_000), false)
        .unwrap();
    assert_eq!(scan.ripe, vec![(digest, size)]);
    assert_eq!(reg.collect_blob(&digest, at(90_000)).unwrap(), Some(size));
    assert!(reg.blob_metadata(&digest).unwrap().is_none(), "L is gone");
    assert!(
        reg.engine()
            .get(&keys::blob_mark(&digest))
            .unwrap()
            .is_none(),
        "the mark goes with it, or the next pass reclaims a resurrected blob"
    );
}

/// The mark is retracted by the writers, not only by purge. A mount adds `P`
/// and no `R`, so if `commit_blob` did not retract it the pass would reclaim
/// bytes a client had just been promised.
#[test]
fn a_reference_or_a_mount_retracts_the_mark() {
    let (_dir, reg) = fixture();
    let (digest, _) = upload(&reg, "demo/app", "layer");
    reg.sweep_repo_blobs(None, 10, at(2_000), false).unwrap();
    reg.scan_blobs(None, 10, at(2_000), at(2_000), false)
        .unwrap();
    assert!(marked(&reg, &digest), "unreferenced, so marked");

    // A mount is one `P` edge and no reference at all.
    reg.commit_blob("other/app", &digest, 5, at(3_000)).unwrap();
    assert!(!marked(&reg, &digest), "the mount retracted it");
    assert_eq!(
        reg.collect_blob(&digest, at(90_000)).unwrap(),
        None,
        "no mark, no collection, however old the blob is"
    );

    // And a manifest push, which is the other way a blob acquires a reason to
    // exist. It goes into the repository the mount put it in - the sweep took
    // its membership of the first one, and a manifest may only name a blob its
    // own repository holds.
    reg.scan_blobs(None, 10, at(4_000), at(4_000), false)
        .unwrap();
    assert!(marked(&reg, &digest));
    let config = upload(&reg, "other/app", "config");
    let body = Image::new(config).layer((digest, 5)).json();
    put(&reg, "other/app", "v1", &body, 5_000);
    assert!(!marked(&reg, &digest), "the push retracted it");

    // A later scan finds the reference and tidies a stale mark rather than
    // leaving it to mature over a live blob.
    reg.engine()
        .apply(&mark_batch(&digest))
        .expect("plant a stale mark");
    let scan = reg
        .scan_blobs(None, 10, at(6_000), at(6_000), false)
        .unwrap();
    assert_eq!(scan.unmarked, 1);
    assert!(!marked(&reg, &digest));
}

/// Retracting a membership restarts the blob's clock.
///
/// This is what keeps a push safe without a lock: the only blob a push may
/// name is one its repository holds, so a membership disappearing has to be
/// treated as somebody having touched the blob a moment ago.
#[test]
fn retracting_a_membership_clears_the_mark() {
    let (_dir, reg) = fixture();
    let (digest, _) = upload(&reg, "demo/app", "layer");

    // A mark from an earlier pass, standing over a blob that still has a
    // membership - a mount, say, that nobody has followed up on.
    reg.engine().apply(&mark_batch(&digest)).unwrap();
    assert!(marked(&reg, &digest));

    let step = reg.sweep_repo_blobs(None, 10, at(2_000), false).unwrap();
    assert_eq!(step.retracted, 1, "the retraction is counted once");
    assert!(
        !marked(&reg, &digest),
        "and it took the mark with it, or the very next pass could collect \
         bytes a push is halfway through referencing"
    );
    assert_eq!(
        reg.collect_blob(&digest, at(90_000)).unwrap(),
        None,
        "so the clock starts again from here"
    );

    // The client-driven spelling of the same thing: `DELETE
    // /v2/<name>/blobs/<digest>` drops a membership too.
    reg.commit_blob("demo/app", &digest, 5, at(3_000)).unwrap();
    reg.engine().apply(&mark_batch(&digest)).unwrap();
    reg.delete_blob_reference("demo/app", &digest).unwrap();
    assert!(!marked(&reg, &digest), "the endpoint clears it as well");
}

/// A blob a manifest still names is never touched, whatever the clocks say.
#[test]
fn a_referenced_blob_survives_every_pass() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let layer = upload(&reg, "demo/app", "layer");
    let body = Image::new(config).layer(layer).json();
    put(&reg, "demo/app", "v1", &body, 2_000);

    let step = reg
        .sweep_repo_blobs(None, 10, at(900_000), false)
        .expect("sweep");
    assert_eq!(step.retracted, 0, "every membership has an R edge");

    let scan = reg
        .scan_blobs(None, 10, at(900_000), at(900_000), false)
        .unwrap();
    assert_eq!((scan.marked, scan.ripe.len()), (0, 0));
    assert_eq!(reg.collect_blob(&layer.0, at(900_000)).unwrap(), None);
}

/// The three guards, each on its own: an index's child, a signature over a
/// subject that is still there, and a manifest pushed a moment ago.
#[test]
fn the_untagged_pass_declines_children_referrers_and_fresh_pushes() {
    let (_dir, reg) = fixture();

    // A multi-arch push: the index is tagged, the children never are.
    let child = {
        let config = upload(&reg, "demo/app", "amd64-config");
        let body = Image::new(config).json();
        let digest = put(&reg, "demo/app", &sha256(&body).to_string(), &body, 1_000);
        (digest, body.len() as u64)
    };
    let index = index_json(&[(child.0, child.1, "amd64")]);
    put(&reg, "demo/app", "v1", &index, 1_000);

    // A signature: a manifest with a subject, tagged only by cosign's
    // convention, which `stage_set_tag` turns into an `F` edge.
    let subject = {
        let config = upload(&reg, "demo/app", "subject-config");
        let body = Image::new(config).json();
        let digest = put(&reg, "demo/app", "signed", &body, 1_000);
        (digest, body.len() as u64)
    };
    let signature = {
        let config = upload(&reg, "demo/app", "sig-config");
        let body = Image::new(config).subject(subject).json();
        put(&reg, "demo/app", &sha256(&body).to_string(), &body, 1_000)
    };

    // And something pushed just now with no tag at all.
    let fresh = {
        let config = upload(&reg, "demo/app", "fresh-config");
        let body = Image::new(config).json();
        put(
            &reg,
            "demo/app",
            &sha256(&body).to_string(),
            &body,
            9_000_000,
        )
    };

    let scan = reg
        .purgeable_manifests("demo/app", None, 100, at(5_000_000))
        .expect("scan");
    assert!(
        scan.digests.is_empty(),
        "nothing here is garbage: {:?}",
        scan.digests
    );
    for (what, digest) in [
        ("child", child.0),
        ("signature", signature),
        ("fresh", fresh),
    ] {
        assert_eq!(
            reg.purge_manifest("demo/app", &digest, at(5_000_000), at(5_000_000))
                .unwrap(),
            None,
            "the {what} must survive"
        );
    }

    // Delete the subject and the signature stops being pinned - which is the
    // cosign leak the schema synthesises `F` edges to close.
    reg.delete_manifest("demo/app", &subject.0, at(6_000_000))
        .unwrap();
    let scan = reg
        .purgeable_manifests("demo/app", None, 100, at(7_000_000))
        .expect("scan");
    assert_eq!(
        scan.digests,
        vec![signature],
        "the signature is now garbage"
    );
}

/// An untagged manifest goes, and takes exactly what a `DELETE` would.
#[test]
fn the_untagged_pass_reclaims_a_manifest_and_leaves_its_layers_collectable() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let layer = upload(&reg, "demo/app", "layer");
    let body = Image::new(config).layer(layer).json();
    let digest = put(&reg, "demo/app", "v1", &body, 1_000);
    reg.delete_tag("demo/app", "v1", at(2_000)).unwrap();

    let deleted = reg
        .purge_manifest("demo/app", &digest, at(500_000), at(500_000))
        .unwrap()
        .expect("untagged and old enough");
    assert_eq!(deleted.digest, digest);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert!(
        !reg.engine()
            .exists_prefix(&keys::blob_ref(&layer.0, repo, &digest))
            .unwrap(),
        "the R edge went with the manifest, which is what frees the layer"
    );

    // And the layer now walks the ordinary blob path: membership, mark, bytes.
    assert_eq!(
        reg.sweep_repo_blobs(None, 10, at(500_000), false)
            .unwrap()
            .retracted,
        2,
        "config and layer"
    );
    let scan = reg
        .scan_blobs(None, 10, at(500_000), at(500_000), false)
        .unwrap();
    assert_eq!(scan.marked, 2);
}

/// Idleness, not age: a slow chunked push touches its session and must survive
/// however long the whole upload takes.
#[test]
fn an_upload_expires_on_idleness_rather_than_on_age() {
    let (_dir, reg) = fixture();
    let slow = [1u8; 16];
    let abandoned = [2u8; 16];
    reg.create_upload("demo/app", &slow, "sha256", at(1_000))
        .unwrap();
    reg.create_upload("demo/app", &abandoned, "sha256", at(1_000))
        .unwrap();

    let mut session = reg.get_upload(&slow).unwrap().expect("session");
    session.offset = 4096;
    session.updated_at = 900;
    reg.save_upload(&slow, &session).unwrap();

    let expired = reg.expired_uploads(at(600_000), 100).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].id, abandoned, "the one nobody touched");

    let expired = reg.expired_uploads(at(1_000_000), 100).unwrap();
    assert_eq!(expired.len(), 2, "eventually even the slow one gives up");
}

/// A name interned by an upload nobody finished, and the four things that keep
/// one alive.
#[test]
fn an_empty_name_is_retired_only_once_it_has_outlived_a_pass() {
    let (_dir, reg) = fixture();
    let id = [7u8; 16];
    reg.create_upload("demo/ghost", &id, "sha256", at(1_000))
        .unwrap();
    let ghost = reg.lookup_repo("demo/ghost").unwrap().unwrap();

    // The upload is in flight, so the name is in use even though nothing is
    // under it.
    let live = reg.live_upload_repos(100).unwrap();
    let watermark = reg.next_repo_id().unwrap();
    assert!(
        reg.empty_repos(None, 100, &live, watermark)
            .unwrap()
            .repos
            .is_empty(),
        "an unfinished upload holds its name open"
    );

    reg.delete_upload(&id).unwrap();
    let scan = reg.empty_repos(None, 100, &[], watermark).unwrap();
    assert_eq!(scan.repos.len(), 1);
    assert_eq!(scan.repos[0].id, ghost);

    // A name interned since the watermark is left alone however empty it looks:
    // this is the intern-then-write window, which no lock in this crate covers.
    let fresh = [8u8; 16];
    reg.create_upload("demo/new", &fresh, "sha256", at(2_000))
        .unwrap();
    reg.delete_upload(&fresh).unwrap();
    let names: Vec<_> = reg
        .empty_repos(None, 100, &[], watermark)
        .unwrap()
        .repos
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["demo/ghost"], "the newer name is not eligible yet");
    assert_eq!(
        reg.retire_repo("demo/new", &[], watermark).unwrap(),
        None,
        "and it cannot be retired directly either"
    );

    assert_eq!(
        reg.retire_repo("demo/ghost", &[], watermark).unwrap(),
        Some(ghost)
    );
    assert_eq!(
        reg.lookup_repo("demo/ghost").unwrap(),
        None,
        "name released"
    );
}

/// History outlives what it describes, so a repository that ever had a tag
/// keeps its name even when everything under it has been deleted.
#[test]
fn a_name_with_history_is_never_retired() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let digest = put(&reg, "demo/app", "v1", &body, 1_000);
    reg.delete_manifest("demo/app", &digest, at(2_000)).unwrap();
    reg.delete_blob_reference("demo/app", &config.0).unwrap();

    let watermark = reg.next_repo_id().unwrap();
    let scan = reg.empty_repos(None, 100, &[], watermark).unwrap();
    assert!(
        scan.repos.is_empty(),
        "tag history is still queryable and reaching it needs the name"
    );
    assert_eq!(reg.retire_repo("demo/app", &[], watermark).unwrap(), None);
}

/// A dry run answers the same questions and writes nothing.
#[test]
fn a_dry_run_touches_nothing() {
    let (_dir, reg) = fixture();
    let (digest, size) = upload(&reg, "demo/app", "orphan");
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    let step = reg.sweep_repo_blobs(None, 10, at(2_000), true).unwrap();
    assert_eq!(step.retracted, 1, "it still says what it would retract");
    assert!(
        reg.engine()
            .exists_prefix(&keys::repo_blob(repo, &digest))
            .unwrap(),
        "and the membership is still there"
    );

    let scan = reg
        .scan_blobs(None, 10, at(2_000), at(2_000), true)
        .unwrap();
    assert_eq!(scan.marked, 1, "it would mark this blob");
    assert!(!marked(&reg, &digest), "but it did not");

    // With a mark already standing, a dry run reports the bytes it would take.
    reg.engine().apply(&mark_batch(&digest)).unwrap();
    let scan = reg
        .scan_blobs(None, 10, at(90_000), at(90_000), true)
        .unwrap();
    assert_eq!(scan.ripe, vec![(digest, size)]);
    assert!(reg.blob_metadata(&digest).unwrap().is_some(), "still there");
}

fn marked(reg: &Registry, digest: &Digest) -> bool {
    reg.engine()
        .get(&keys::blob_mark(digest))
        .unwrap()
        .is_some()
}

/// A mark as purge would have written it at the epoch, for tests that need one
/// to already be standing.
fn mark_batch(digest: &Digest) -> summ_meta::WriteBatch {
    let mut batch = summ_meta::WriteBatch::new();
    batch.put(
        keys::blob_mark(digest),
        postcard::to_allocvec(&summ_core::BlobMark { seen_at: 1 }).unwrap(),
    );
    batch
}
