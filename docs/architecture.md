# Architecture

summ is one process with two stores: an embedded RocksDB database for metadata
and a content-addressed file tree for blob bytes. The design premise is that a
cold `containerd` pull spends four of its five serial steps on metadata
lookups, so the metadata path is where a registry is fast or slow.

## Request path

```
client ──HTTP──▶ summ-server ──▶ summ-registry ──▶ summ-meta (RocksDB)   meta/
                     │                                 
                     └──────────▶ summ-storage (files)                  blobs/
```

| Crate | Role |
|---|---|
| `summ-core` | Types, digests, and the binary key schema shared by every layer |
| `summ-meta` | The `MetaEngine` trait and its RocksDB implementation |
| `summ-storage` | The blob store: `digest -> bytes` on the filesystem, nothing else |
| `summ-registry` | Turns each Distribution Spec operation into one atomic write batch |
| `summ-server` | HTTP handlers, auth, pull counters, the discovery API, the UI |

The HTTP layer talks to the lower crates through one trait, and an in-memory
implementation of that trait backs the handler tests. The server also carries
an in-memory implementation so spec behaviour is tested without a disk.

## The metadata engine

**RocksDB, embedded and statically linked.** There is no database process to
run and no engine to choose. A second engine, redb, exists only in the test
build to keep the engine trait honest.

**Why an LSM.** A push inserts tens of keys at effectively random positions,
because keys are digest-prefixed. A log-structured merge tree absorbs random
inserts into sequential writes. Deletes become tombstones, which makes purge
cheap. Block compression pays off unusually well because most of the keyspace
is valueless edge keys sharing long digest prefixes.

**The key schema is the product.** Every key starts with a one-byte type
prefix: uppercase for registry data, lowercase for internal bookkeeping.
Repository names are interned to a 4-byte id so a long name is not repeated in
every key. Digests are stored raw, not hex. Values are postcard-encoded, and an
edge key that only needs to exist carries no value at all. Every type:

| Prefix | Entity | Key | Value | Answers |
|---|---|---|---|---|
| `M` | Manifest | repo, digest | `ManifestRecord`: media type, own size, layer total, platform, layers, children, subject, artifact type, annotations, push time | what this manifest is, without decoding its JSON |
| `B` | Manifest body | repo, digest | the manifest JSON, zstd-compressed | the exact bytes a manifest `GET` returns |
| `T` | Tag | repo, tag | `TagRecord`: digest, tagged time | which digest a tag points at, sorted by tag name |
| `G` | Manifest tag edge | repo, digest, tag | — | which tags point at a manifest, and so whether it is purgeable |
| `L` | Blob | digest | `BlobRecord`: size | blob exists registry-wide, and its size |
| `R` | Blob reference edge | digest, repo, manifest | — | which manifests reference a blob |
| `P` | Repo blob | repo, digest | `RepoBlobRecord`: size, added time | blob is in this repo; the grace clock purge reads |
| `S` | Child parent edge | repo, child, parent | — | which indexes list a per-platform manifest |
| `F` | Referrer edge | repo, subject, referrer | `ReferrerRecord`: media type, artifact type, size, annotations | OCI 1.1 referrers, filtered during the scan |
| `U` | Upload session | uuid | `UploadSession`: repo, offset, timestamps, digest algorithm, hasher state | where a chunked upload resumes, on any process |
| `H` | Tag event, by tag | repo, tag, `!`time, digest | `TagEvent`: created or deleted, media type, size | one tag's history, newest first |
| `J` | Tag event, by manifest | repo, digest, `!`time, tag | `TagEvent` | what a manifest was ever tagged, and when |
| `A` | Counter bucket | scope, repo, subject (none at repo scope), day, shard | `CounterBucket`: manifest pulls, blob pulls, bytes out, each per hour | pull counters per repo, tag, and manifest |
| `D` | Dead repo | repo id | `DeadRepo`: name, dropped time | the sweeper's worklist after a repository delete |
| `n` | Repo name to id | name | repo id | the interner, and the name order `_catalog` pages in |
| `i` | Repo id to name | repo id | name | an id back to the name a response prints |
| `v` | Schema version | — | `SCHEMA_VERSION` | whether this build may open this store |

Timestamps in `H` and `J` keys are stored complemented, written `!`time above,
so a forward scan arrives newest first. `A` keys carry a writing-node shard so
two nodes cannot last-write-wins over one bucket.

Three rules follow from the schema:

- **Nothing is a directory walk.** The catalog, a tag list, a referrers query,
  and "is this blob still referenced" are each a prefix seek.
- **No value grows with the registry.** Fan-in relationships are one key per
  edge, not a list inside a value. A base layer referenced by a million
  manifests costs a million small keys, and adding one more is an O(1) insert.
  There is deliberately no read-modify-write primitive in the engine.
- **Every mutation is one atomic batch.** A manifest push writes the record,
  the body, a reference edge per layer, and the tag, and the batch lands whole
  or not at all. Batches are value-oriented and serialisable, which is the seam
  a future replica would consume.

**Tuning.** A prefix extractor groups keys by their meaningful prefix, so
prefix bloom filters answer the hot existence checks without touching an SST.
The engine name is versioned with the key layout so RocksDB never trusts a
filter built under old rules. A schema version marker in the store makes an
incompatible build refuse to open rather than return undecodable records.

**Pagination.** Every list endpoint, in `/v2/` and in `/api/v1/`, takes a
cursor and a limit. The design target is 10 million repositories with up to 10
million manifests in one, so nothing materialises an unbounded set. An oversized
`?n=` is clamped rather than rejected.

## The blob store

`blobs/<algo>/ab/cd/ef/<full-hex>`: the path is a function of the digest and
the file is the blob. No path encodes a relationship, and nothing lists a
directory to answer a question. The relationships that other registries encode
as link files on disk live in RocksDB as `P`, `R`, `T` and `G` keys, so there
is one source of truth.

Uploads stream straight to a staging file under `uploads/`. The digest is
computed as bytes arrive and never by re-reading the stored blob. Commit
compares the running digest to the client's, then renames the file into the
tree. A pull streams the file in 1 MiB reads and honours range requests, so a
client that opens a range and drops the connection costs nothing more.

## Write ordering

**Bytes land before metadata.** Every write path fsyncs the blob, and the
directory holding it, before the metadata batch commits. The two failure modes
are not symmetric: a blob with no metadata is garbage that purge reclaims, while
metadata naming a missing blob is a corrupt image that fails on pull days
later. A manifest push obeys the same order for its own document.

This is why `meta/`, `blobs/` and `uploads/` must share a filesystem. See
[Data directory](data-dir.md).

## Pull counters

Serving a pull never writes to RocksDB. A `GET` adds to a map in memory, and a
background task folds the map into `A` keys every few seconds. Counters are kept
at every scope that will be queried, per repo, per tag, and per manifest, so a
repo total is a lookup rather than a scan across its manifests.
`--no-pull-counts` stops recording; the API keeps serving what was recorded.

## Discovery API and UI

`/api/v1/` is a flat, read-only, cursor-paged surface: repositories, tags,
manifests, tag history, and pull counts. Each collection is a top-level
resource with the repository name after it, so a name containing `/` is never
ambiguous. A single manifest is addressed as `<name>@<tag-or-digest>`.

The UI is compiled into the binary and served from the same port, with no
build step and no CDN, so it works air-gapped. It reads through `/api/v1/` and
deletes a repository through `/v2/`, so it sits behind the same auth as every
other client. On an open registry the delete button works for anyone who can
reach the port.

## Conformance

The OCI `distribution-spec` conformance suite passes with zero failures at
every profile, including the OCI 1.1 referrers API.
