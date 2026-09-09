//! Purge: reclaiming what the deletes leave behind.
//!
//! Every delete in this crate is a metadata operation. `DELETE` on a manifest
//! retracts its edges, `DELETE` on a repository releases the name and sweeps
//! its keys, and neither touches a byte of content - a layer is shared
//! registry-wide, so whether one repository was its last user is a question
//! about the whole store rather than about the operation in hand. This module
//! is where that question is asked.
//!
//! Five passes, each a bounded resumable step over one key range:
//!
//! 1. [`Registry::purgeable_manifests`] / [`Registry::purge_manifest`] - the
//!    untagged set, opt-in, guarded.
//! 2. [`Registry::sweep_repo_blobs`] - `P` memberships whose manifest never
//!    arrived.
//! 3. [`Registry::scan_blobs`] / [`Registry::collect_blob`] - the marks, and
//!    the blobs whose marks have matured.
//! 4. [`Registry::expired_uploads`] - abandoned sessions.
//! 5. [`Registry::empty_repos`] / [`Registry::retire_repo`] - names with
//!    nothing under them.
//!
//! Ordering matters between them and is the caller's to keep: each stage
//! releases work for the next, so one pass in this order carries a repository
//! delete all the way to reclaimed bytes rather than needing five.
//!
//! # Why the blob pass needs a mark
//!
//! "Is this blob still referenced" is one seek - `exists_prefix` over `R
//! <digest>` - and that is what the key schema was shaped for. "Does any
//! repository still hold it" is not askable at all: `P` is keyed `<repo>
//! <digest>`, so the memberships of one blob are scattered across the range in
//! repository order. Nor can the pass be driven by watching memberships
//! disappear, because [`Registry::finish_repo_sweep`] drops a dead
//! repository's whole `P` range with a single `DeletePrefix` and never
//! enumerates it.
//!
//! So the blob pass walks `B`, and the gap it has to cover is the mount:
//! [`Registry::commit_blob`] adds a membership under a second name without
//! writing any `R` edge, so a blob mounted a moment ago looks exactly like a
//! blob nothing has wanted for a year. `C <digest>` closes it. Purge writes the
//! mark the first time it finds a blob unreferenced; the mark has to stand for
//! a whole grace period before the bytes go, and every path that creates a
//! reference or a membership retracts it in the batch it was already writing.
//!
//! # Why a push cannot lose its layers to a pass
//!
//! A manifest may only name a blob its own repository holds - `validate_references`
//! is on by default - so a push that plans successfully saw either an `R` edge
//! or a live `P` for every layer. Both are covered:
//!
//! - **An `R` edge** stops the collection outright, and the push that would add
//!   one retracts the mark in the same batch.
//! - **A live `P`** was written by a commit or a mount, which retracts the mark;
//!   and when pass 2 later retracts that membership it clears the mark again.
//!   So the clock a blob is collected on cannot have been running while a
//!   membership existed, and a push has milliseconds between its plan and its
//!   apply where a collection is a whole grace period away.
//!
//! This is why the manifest push takes no lock against purge, and why the one
//! lock that does exist - a digest's, taken by `commit_blob` and by the
//! collection - guards exactly one thing: bytes being renamed into place while
//! the pass is deciding to remove them.
//!
//! # What is not here
//!
//! **The archive copies.** `summ-server` writes every manifest document into
//! the blob store as a disaster-recovery corpus, deliberately without a `B` or
//! a `P` record - a manifest is not a blob of its repository. This pass walks
//! `B`, so it never touches one, which is the safe half of that decision; the
//! unsafe half is that a copy whose manifest has been deleted is never
//! reclaimed either. Finding those means walking `blobs/` and asking `M` about
//! every file, and "does any repository hold this manifest digest" is not
//! askable in this schema any more than the membership question is. It belongs
//! with the orphan-file scrub, which is its own opt-in pass and is not here.
//!
//! One consequence worth stating because it is silent: a digest that is *both*
//! a pushed layer and a pushed manifest has one file, and this pass may reclaim
//! it as the layer, which drops that manifest from the corpus. The read path is
//! untouched - `Z` holds the document - and pushing a manifest document as a
//! layer is pathological, but it is a hole in the corpus rather than a hole in
//! the registry, and it closes when the scrub gets its index.
//!
//! Nothing in this module deletes a file. The metadata batch commits first and
//! the caller removes the bytes afterwards, for the same reason a push fsyncs
//! bytes before it commits metadata: the two failure modes are not symmetric.
//! A `B` record whose file is gone is a pull that fails. A file whose `B`
//! record is gone is inert - nothing reaches a blob except through `B`, and a
//! re-push of the same digest makes it live again, the bytes being named by
//! their content. Inert is not reclaimed, though: this pass finds its work by
//! walking `B`, so a file whose record it has already retracted is one it can
//! no longer see, and a crash between the two steps leaks those bytes until
//! the orphan-file scrub above exists. A `P` membership left pointing at
//! reclaimed content is the genuinely self-correcting case - `servable_blob`
//! reads `B` for the size and returns `None` without it, which is a clean
//! `BLOB_UNKNOWN`, and pass 2 collects the membership on a later run.

use summ_core::{
    keys, BlobMark, BlobRecord, Digest, ManifestRecord, RepoBlobRecord, RepoId, Timestamp,
    UploadSession,
};
use summ_meta::WriteBatch;

use crate::codec::{decode, encode};
use crate::delete::ManifestDeleted;
use crate::error::{RegistryError, Result};
use crate::registry::Registry;
use crate::suffix;
use crate::uploads::UploadKey;

/// One page of the untagged-manifest scan.
///
/// Like the filtered referrers query and `untagged_manifests`, the cursor
/// advances over `M` rather than over the results, so a page may come back
/// empty with `next` still set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestScan {
    /// Manifests that passed every guard at scan time. The caller re-checks
    /// under the repository lock before deleting one.
    pub digests: Vec<Digest>,
    pub examined: usize,
    pub next: Option<Vec<u8>>,
}

/// One step of the membership sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipSweep {
    pub examined: usize,
    pub retracted: usize,
    pub next: Option<Vec<u8>>,
}

/// One step of the blob scan: what it marked, what it unmarked, and what is
/// ripe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobScan {
    pub examined: usize,
    /// Blobs seen unreferenced for the first time, and now marked.
    pub marked: usize,
    /// Blobs whose mark was retracted because a reference exists again. Purge
    /// clears these itself as well as every writer doing so, because a mark
    /// that outlived its reason would collect a live blob one grace period
    /// later.
    pub unmarked: usize,
    /// Marks that have stood for the whole grace period, with the size each
    /// blob accounts for. Candidates only: [`Registry::collect_blob`] re-checks
    /// every one of them under the caller's lock, and the size is here so a
    /// dry run can report bytes without a second lookup.
    pub ripe: Vec<(Digest, u64)>,
    pub next: Option<Vec<u8>>,
}

/// An upload session that has not been touched inside the TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredUpload {
    pub id: UploadKey,
    pub repo: RepoId,
    pub updated_at: u64,
}

/// One page of the empty-repository scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmptyRepoScan {
    pub repos: Vec<EmptyRepo>,
    pub examined: usize,
    /// The last name examined, not the last one returned - the scan skips
    /// rows, so where it got to is not recoverable from what it served.
    pub next: Option<String>,
}

/// A repository name with nothing under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyRepo {
    pub id: RepoId,
    pub name: String,
}

// --- 1. untagged manifests ---------------------------------------------

impl Registry {
    /// One page of manifests this repository could reclaim, if the operator
    /// has asked for untagged manifests to be reclaimed at all.
    ///
    /// `M` minus `G` is the reclaimable set - the same predicate as
    /// [`Registry::untagged_manifests`], which is what the discovery API
    /// serves - but "untagged" on its own reclaims things that are still very
    /// much in use, so three further guards apply:
    ///
    /// - **A child of an index.** A multi-arch push tags the index and never
    ///   the per-platform manifests, so every one of them is untagged by
    ///   construction. A non-empty `S <repo> <this> <*>` keeps it.
    /// - **A referrer of a live subject.** A signature is a manifest whose
    ///   `subject` names what it signs, and the subject is what keeps it. This
    ///   covers the legacy cosign tag form too, because `stage_set_tag`
    ///   synthesises the `F` edge and the `subject` alike.
    /// - **Age.** A manifest pushed after `pushed_before` is left alone. A push
    ///   that has not tagged its manifest yet is the ordinary shape of `docker
    ///   push` mid-flight, not garbage.
    ///
    /// What is deliberately *not* a guard: being someone's `subject` while that
    /// someone is gone, and being pulled by digest. The first is what the
    /// referrers spec calls a dangling subject and cannot pin anything; the
    /// second is why this whole pass is opt-in.
    pub fn purgeable_manifests(
        &self,
        repo: &str,
        cursor: Option<&[u8]>,
        limit: usize,
        pushed_before: Timestamp,
    ) -> Result<ManifestScan> {
        let repo_id = self.require_repo(repo)?;
        let page = self
            .engine()
            .scan(&keys::manifests_in_repo(repo_id), cursor, limit)?;

        let mut digests = Vec::new();
        for (_, raw) in &page.entries {
            let record: ManifestRecord = decode(raw, "ManifestRecord")?;
            if self.manifest_is_purgeable(repo_id, &record, pushed_before)? {
                digests.push(record.digest);
            }
        }
        Ok(ManifestScan {
            digests,
            examined: page.entries.len(),
            next: page.next,
        })
    }

    /// Delete one untagged manifest, or decline to.
    ///
    /// The guards are checked again here rather than trusted from the scan,
    /// because the caller holds the repository's lock by this point and the
    /// scan did not: a tag pushed in between is exactly the interleaving that
    /// would otherwise reclaim a manifest somebody had just named. `Ok(None)`
    /// means it no longer qualifies, which is a normal outcome and not an
    /// error.
    ///
    /// The deletion itself is [`Registry::plan_manifest_delete`] unchanged -
    /// the same batch `DELETE /v2/<name>/manifests/<digest>` applies, cascade
    /// and all. There is no second implementation of what it means to remove a
    /// manifest.
    pub fn purge_manifest(
        &self,
        repo: &str,
        digest: &Digest,
        pushed_before: Timestamp,
        now: Timestamp,
    ) -> Result<Option<ManifestDeleted>> {
        let repo_id = self.require_repo(repo)?;
        let Some(record) = self.manifest_record(repo_id, digest)? else {
            return Ok(None);
        };
        if !self.manifest_is_purgeable(repo_id, &record, pushed_before)? {
            return Ok(None);
        }
        let planned = self.plan_manifest_delete(repo, digest, now)?;
        self.engine().apply(&planned.batch)?;
        Ok(Some(planned.outcome))
    }

    fn manifest_is_purgeable(
        &self,
        repo_id: RepoId,
        record: &ManifestRecord,
        pushed_before: Timestamp,
    ) -> Result<bool> {
        if record.pushed_at > pushed_before.secs() {
            return Ok(false);
        }
        if self
            .engine()
            .exists_prefix(&keys::tags_of_manifest(repo_id, &record.digest))?
        {
            return Ok(false);
        }
        if self
            .engine()
            .exists_prefix(&keys::parents_of(repo_id, &record.digest))?
        {
            return Ok(false);
        }
        if let Some(subject) = record.subject {
            if self
                .engine()
                .exists_prefix(&keys::manifest(repo_id, &subject))?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

// --- 2. stale memberships ----------------------------------------------

impl Registry {
    /// Retract `P` memberships whose manifest never arrived.
    ///
    /// A blob is added to a repository's set the moment its bytes land, before
    /// any manifest references it - that is what makes a two-step push work at
    /// all. `added_at` is the clock on that promise: a membership with no `R`
    /// edge in its own repository, older than `added_before`, is an upload
    /// whose manifest is never coming, and `plan_blob_commit` refuses to
    /// restart the clock on re-upload precisely so this stays true.
    ///
    /// One scan over the whole `P` range rather than a loop over repositories.
    /// The key carries the repository id, so the pass needs no names, no
    /// interner lookups and no locks - the worst a lost race can do is retract
    /// a membership a mount added moments ago, which costs that client a
    /// re-mount and cannot cost anyone bytes, because bytes go only when no `R`
    /// edge exists anywhere and a mark has stood for a full grace period.
    ///
    /// **A retraction also clears the blob's mark**, and that is what makes the
    /// whole design safe against a push. See the module docs: a manifest may
    /// only name a blob its repository holds, so a push that plans
    /// successfully saw either an `R` edge - which stops any collection - or a
    /// live `P`. Clearing the mark here means a `P` that goes away restarts the
    /// clock, so the blob cannot be collected until a further grace period has
    /// passed with nothing touching it at all. A push has milliseconds between
    /// its plan and its apply; the collection is a grace period away.
    ///
    /// No `plan_` form, for the same reason [`Registry::sweep_repo_refs`] has
    /// none: the batch is derived from a scan of the range it is emptying, so
    /// it is not something a caller could fold into another operation.
    ///
    /// `report_only` counts what it would retract and writes nothing, which is
    /// what `?dry-run` serves.
    pub fn sweep_repo_blobs(
        &self,
        cursor: Option<&[u8]>,
        limit: usize,
        added_before: Timestamp,
        report_only: bool,
    ) -> Result<MembershipSweep> {
        let page = self.engine().scan(&keys::repo_blobs(), cursor, limit)?;

        let mut batch = WriteBatch::new();
        let mut retracted = 0usize;
        for (key, raw) in &page.entries {
            let repo = repo_of_membership(key)?;
            let digest =
                suffix::digest_after_repo(key).ok_or_else(|| RegistryError::corrupt("P key"))?;
            let record: RepoBlobRecord = decode(raw, "RepoBlobRecord")?;
            if record.added_at > added_before.secs() {
                continue;
            }
            if self
                .engine()
                .exists_prefix(&keys::blob_refs_in_repo(&digest, repo))?
            {
                continue;
            }
            batch.delete(key.clone());
            batch.delete(keys::blob_mark(&digest));
            retracted += 1;
        }
        if !report_only && !batch.is_empty() {
            self.engine().apply(&batch)?;
        }

        Ok(MembershipSweep {
            examined: page.entries.len(),
            retracted,
            next: page.next,
        })
    }
}

/// The repository id out of a `P <repo> <digest>` key.
fn repo_of_membership(key: &[u8]) -> Result<RepoId> {
    let bytes: [u8; 4] = key
        .get(1..5)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| RegistryError::corrupt("P key"))?;
    Ok(RepoId::from_be_bytes(bytes))
}

// --- 3. blobs ----------------------------------------------------------

impl Registry {
    /// Walk one page of `B`, maintaining the marks and reporting what is ripe.
    ///
    /// Three verdicts per blob, all from one seek over `R <digest>` and one
    /// point lookup of the mark:
    ///
    /// - **Referenced.** Any mark is retracted. Every writer retracts its own,
    ///   so this is belt and braces - but a mark that outlived its reason
    ///   collects a live blob one grace period later, and the seek that would
    ///   have caught it is one this pass is making anyway.
    /// - **Unreferenced and unmarked.** Mark it at `now`. The clock starts
    ///   here, not at the push: what matters is how long the blob has gone
    ///   unwanted.
    /// - **Unreferenced, marked at or before `seen_before`.** Ripe. Returned
    ///   for the caller to confirm and collect under the digest's lock.
    ///
    /// Marking is unlocked. A mark racing a mount's retraction can survive it,
    /// and the blob is then collected a grace period later if - and only if -
    /// no manifest ever referenced it, which is the same outcome as a mount
    /// nobody followed up on. The lock is on the collection, where the cost of
    /// being wrong is bytes rather than a re-mount.
    ///
    /// `report_only` leaves the marks alone. A dry run against a store that has
    /// never been purged therefore reports a great many blobs it *would* mark
    /// and nothing ripe, which is the honest answer: the first real pass
    /// reclaims nothing either, because the clock starts when the mark lands.
    pub fn scan_blobs(
        &self,
        cursor: Option<&[u8]>,
        limit: usize,
        seen_before: Timestamp,
        now: Timestamp,
        report_only: bool,
    ) -> Result<BlobScan> {
        let page = self.engine().scan(&keys::blobs(), cursor, limit)?;

        let mut scan = BlobScan {
            examined: page.entries.len(),
            next: page.next,
            ..BlobScan::default()
        };
        let mut batch = WriteBatch::new();
        for (key, raw) in &page.entries {
            let digest = digest_of_blob_key(key)?;
            let mark = self.blob_mark(&digest)?;
            if self.engine().exists_prefix(&keys::blob_refs(&digest))? {
                if mark.is_some() {
                    batch.delete(keys::blob_mark(&digest));
                    scan.unmarked += 1;
                }
                continue;
            }
            match mark {
                None => {
                    batch.put(
                        keys::blob_mark(&digest),
                        encode(&BlobMark {
                            seen_at: now.secs(),
                        })?,
                    );
                    scan.marked += 1;
                }
                Some(mark) if mark.seen_at <= seen_before.secs() => {
                    let record: BlobRecord = decode(raw, "BlobRecord")?;
                    scan.ripe.push((digest, record.size));
                }
                Some(_) => {}
            }
        }
        if !report_only && !batch.is_empty() {
            self.engine().apply(&batch)?;
        }
        Ok(scan)
    }

    /// Retract a ripe blob's metadata, or decline to. The size it accounted
    /// for is the outcome, so a caller can report reclaimed bytes without a
    /// second lookup.
    ///
    /// Every condition is checked again here, under whatever lock the caller
    /// holds against [`Registry::commit_blob`], because the scan that produced
    /// the candidate held none. `Ok(None)` covers all three ways a candidate
    /// stops qualifying - the record is gone, a reference appeared, the mark
    /// was retracted or reset - and none of them is an error.
    ///
    /// **The bytes are the caller's to remove, and only after this returns.**
    /// A `B` with no file is a pull that fails; a file with no `B` is inert,
    /// live again on a re-push of the same digest and otherwise left for the
    /// orphan-file scrub, because this pass finds candidates by walking `B`
    /// and can no longer see one whose record is gone.
    pub fn collect_blob(&self, digest: &Digest, seen_before: Timestamp) -> Result<Option<u64>> {
        let Some(record) = self.blob_metadata(digest)? else {
            return Ok(None);
        };
        let Some(mark) = self.blob_mark(digest)? else {
            return Ok(None);
        };
        if mark.seen_at > seen_before.secs() {
            return Ok(None);
        }
        if self.engine().exists_prefix(&keys::blob_refs(digest))? {
            return Ok(None);
        }

        let mut batch = WriteBatch::new();
        batch.delete(keys::blob(digest));
        batch.delete(keys::blob_mark(digest));
        self.engine().apply(&batch)?;
        let BlobRecord { size } = record;
        Ok(Some(size))
    }

    pub(crate) fn blob_mark(&self, digest: &Digest) -> Result<Option<BlobMark>> {
        match self.engine().get(&keys::blob_mark(digest))? {
            Some(raw) => Ok(Some(decode(&raw, "BlobMark")?)),
            None => Ok(None),
        }
    }
}

fn digest_of_blob_key(key: &[u8]) -> Result<Digest> {
    key.get(1..)
        .and_then(Digest::decode)
        .map(|(d, _)| d)
        .ok_or_else(|| RegistryError::corrupt("B key"))
}

// --- 4. abandoned uploads ----------------------------------------------

impl Registry {
    /// Sessions untouched since `idle_before`.
    ///
    /// Bounded by concurrency rather than by the size of the registry - `U`
    /// holds one key per upload in flight - which is why this needs no cursor,
    /// exactly as [`Registry::live_upload_repos`] does not.
    ///
    /// `updated_at` and not `started_at`: a genuinely slow chunked push of a
    /// multi-gigabyte layer touches the session on every chunk, and expiring it
    /// on total elapsed time would break the one upload shape that most needs
    /// to survive.
    pub fn expired_uploads(
        &self,
        idle_before: Timestamp,
        limit: usize,
    ) -> Result<Vec<ExpiredUpload>> {
        let page = self.engine().scan(&keys::uploads(), None, limit)?;
        let mut out = Vec::new();
        for (key, raw) in &page.entries {
            let session: UploadSession = decode(raw, "UploadSession")?;
            if session.updated_at > idle_before.secs() {
                continue;
            }
            let id: UploadKey = key
                .get(1..)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| RegistryError::corrupt("U key"))?;
            out.push(ExpiredUpload {
                id,
                repo: session.repo,
                updated_at: session.updated_at,
            });
        }
        Ok(out)
    }
}

// --- 5. empty repositories ---------------------------------------------

impl Registry {
    /// The id the next new repository will be given.
    ///
    /// Purge holds this across passes as a watermark: a name whose id is below
    /// the value read at the start of the *previous* pass was interned before
    /// that pass began, and a name interned since is left alone however empty
    /// it looks. That is what makes [`Registry::retire_repo`] safe against the
    /// gap between interning a name and writing the first key under it - a gap
    /// no lock in this crate covers, because interning is its own batch.
    pub fn next_repo_id(&self) -> Result<RepoId> {
        Ok(self.interner().next_id(self.engine())?)
    }

    /// One page of names with nothing under them.
    ///
    /// A repository is interned by the `POST` that opens an upload, before a
    /// byte has landed. Abandon that upload and the name sits in `_catalog`
    /// for ever describing nothing, which is the case this pass exists for.
    ///
    /// "Nothing under them" is stricter than "no manifests". A repository that
    /// ever had a tag has `H` and `J` events, and history outliving what it
    /// describes is a promise this registry makes; a repository that ever
    /// served a pull has `A` buckets. Retiring the name would stem both, since
    /// every one of those keys is reached through the interner and a recreated
    /// name gets a fresh id. So a name is retired only when `M`, `P`, `H`, `J`
    /// and `A` are all empty beneath it and no upload in flight names it - the
    /// never-finished upload, and nothing else.
    ///
    /// `M` is checked first and on its own for the ordinary repository, so the
    /// pass costs one seek per name and stops there.
    ///
    /// `interned_before` is the watermark from [`Registry::next_repo_id`]: a
    /// name whose id is at or above it is skipped whatever its contents, so a
    /// repository has to have been empty across a whole pass to be retired.
    pub fn empty_repos(
        &self,
        start_after: Option<&str>,
        limit: usize,
        live: &[RepoId],
        interned_before: RepoId,
    ) -> Result<EmptyRepoScan> {
        let cursor = start_after.map(keys::repo_by_name);
        let page = self
            .engine()
            .scan(&keys::repos_by_name(), cursor.as_deref(), limit)?;

        let mut repos = Vec::new();
        let mut last = None;
        for (key, value) in &page.entries {
            let name = keys::parse_repo_name(key)
                .ok_or_else(|| RegistryError::corrupt("repo name key"))?;
            let id = keys::parse_repo_id(value)
                .ok_or_else(|| RegistryError::corrupt("repo id value"))?;
            last = Some(name.to_string());
            if id < interned_before && self.repo_is_empty(id, live)? {
                repos.push(EmptyRepo {
                    id,
                    name: name.to_string(),
                });
            }
        }

        Ok(EmptyRepoScan {
            repos,
            examined: page.entries.len(),
            next: page.next.and(last),
        })
    }

    /// Release an empty repository's name, or decline to.
    ///
    /// Applying rather than planning, because the interner's cache has to be
    /// evicted after the batch and that is not something a caller can do -
    /// same shape as [`Registry::delete_repository`], and for the same reason.
    ///
    /// Nothing is tombstoned: there is by definition nothing under the id to
    /// sweep. The id is not reused either, so a name interned again a moment
    /// later cannot collide with anything.
    pub fn retire_repo(
        &self,
        name: &str,
        live: &[RepoId],
        interned_before: RepoId,
    ) -> Result<Option<RepoId>> {
        let Some(id) = self.lookup_repo(name)? else {
            return Ok(None);
        };
        if id >= interned_before || !self.repo_is_empty(id, live)? {
            return Ok(None);
        }

        let mut batch = WriteBatch::new();
        batch.delete(keys::repo_by_name(name));
        batch.delete(keys::repo_by_id(id));
        self.engine().apply(&batch)?;
        // After the batch, never before: until the mapping is gone, evicting it
        // only forces the next lookup to read it back.
        self.forget_repo(name, id);
        Ok(Some(id))
    }

    fn repo_is_empty(&self, id: RepoId, live: &[RepoId]) -> Result<bool> {
        if live.contains(&id) {
            return Ok(false);
        }
        for prefix in [
            keys::manifests_in_repo(id),
            keys::blobs_in_repo(id),
            keys::tags_in_repo(id),
            keys::tag_history_in_repo(id),
            keys::manifest_tag_history_in_repo(id),
            keys::counters_in_repo_scope(keys::SCOPE_MANIFEST, id),
            keys::counters_in_repo_scope(keys::SCOPE_TAG, id),
            keys::counters_in_repo_scope(keys::SCOPE_REPO, id),
        ] {
            if self.engine().exists_prefix(&prefix)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
