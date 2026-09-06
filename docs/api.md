# HTTP API

summ serves two HTTP surfaces on one port.

| Surface | What it is | Who it is for |
|---|---|---|
| `/v2/` | The [OCI Distribution Spec](https://github.com/opencontainers/distribution-spec/blob/main/spec.md), plus `_catalog` | `docker`, `podman`, `containerd`, `crane`, `oras`, and every other registry client |
| `/api/v1/` | summ's own read-only discovery API, and one delete | The built-in UI, dashboards, cleanup scripts |

Both are governed by the same `--auth-mode` — there is no exemption list. `GET`,
`HEAD` and `OPTIONS` are reads; every other method is a write. See
[Authentication](auth.md).

Errors on both surfaces use the spec's envelope:

```json
{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known to registry","detail":"demo/app"}]}
```

## The registry API — `/v2/`

summ implements the Distribution Spec in full; the conformance suite passes at
every profile, including the OCI 1.1 referrers API. The spec is the reference
for request and response detail, so this is a map rather than a copy of it.
The `end-N` ids link to the spec's own endpoint table.

| Endpoint | Methods | Operation |
|---|---|---|
| `/v2/` | `GET` | API version check — `200` means the client may proceed ([end-1]) |
| `/v2/<name>/blobs/<digest>` | `GET`, `HEAD`, `DELETE` | Pull a blob, check it exists, delete it ([end-2], [end-10]) |
| `/v2/<name>/manifests/<reference>` | `GET`, `HEAD`, `PUT`, `DELETE` | Pull, check, push, or delete a manifest by tag or digest ([end-3], [end-7], [end-9]) |
| `/v2/<name>/blobs/uploads/` | `POST` | Start an upload, or push a blob in one request, or mount one from another repository ([end-4], [end-11]) |
| `/v2/<name>/blobs/uploads/<id>` | `PATCH`, `PUT`, `GET`, `DELETE` | Send a chunk, finish, ask the offset, cancel ([end-5], [end-6], [end-13], [end-14]) |
| `/v2/<name>/tags/list` | `GET`, `HEAD` | Tags in byte order, paged with `?n=` and `?last=` ([end-8]) |
| `/v2/<name>/referrers/<digest>` | `GET`, `HEAD` | Manifests whose `subject` is this digest, filterable with `?artifactType=` ([end-12]) |
| `/v2/_catalog` | `GET`, `HEAD` | Repository names, paged. Not a spec endpoint — see below |

[end-1]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#endpoints
[end-2]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-blobs
[end-3]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-manifests
[end-4]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#post-then-put
[end-5]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pushing-a-blob-in-chunks
[end-6]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pushing-a-blob-in-chunks
[end-7]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pushing-manifests
[end-8]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#listing-tags
[end-9]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#deleting-manifests
[end-10]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#deleting-blobs
[end-11]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#mounting-a-blob-from-another-repository
[end-12]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#listing-referrers
[end-13]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pushing-a-blob-in-chunks
[end-14]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#canceling-a-blob-upload

A repository name may contain `/`, and summ imposes no depth limit: `a/b/c/d/e`
is one name. Nothing in the route table caps how many components a name has.

Things worth knowing that the spec leaves open:

- **`?n=` is clamped, never rejected.** The ceiling is `--max-page-size`
  (default 1000) and the default page is `--default-page-size`. The spec permits
  returning fewer results than asked for as long as a `Link` header follows, so
  an oversized `n` gets a full page and a cursor. Only a malformed `n` is a
  `400`, carrying `PAGINATION_NUMBER_INVALID` — the one code summ sends that is
  outside the spec's closed set, matched to what the reference implementation
  already sends for the same condition.
- **`Link` appears only when a further page exists.** The key range is ordered,
  so summ peeks one key past the page rather than guessing from a full page.
- **`?n=0`** returns an empty list and no `Link`, but still `404`s an unknown
  repository.
- **`PUT` accepts extra `?tag=` parameters** (end-7b), up to 32 of them; each is
  one more key in the same atomic batch. Above the limit, `414`.
- **A manifest above `--max-manifest-bytes`** (default 8 MiB) is a `413`, and an
  upload body above `--max-upload-bytes` is a `413 SIZE_INVALID`.
- **Referrers never `404`.** Once the endpoint is enabled, an unknown subject —
  or an unknown repository — is `200` with an empty `manifests` array, because a
  referrer and its subject may be pushed in either order. Only a malformed
  digest is a `400`. `--no-referrers` hides the endpoint; the index is written
  either way. A filtered response carries `OCI-Filters-Applied: artifactType`.
- **summ never sends `429`.** containerd retries a `429` immediately and without
  honouring `Retry-After`, which multiplies the load it was meant to shed.
- **`_catalog` is not in the spec** — it was removed before v1.0.0 and the
  conformance suite never calls it. summ implements it because every client
  expects it, with the pagination rules above.
- **Deleting a manifest is `202`,** and by tag as well as by digest. Blob bytes
  are reclaimed by purge, not by the delete.

## The discovery API — `/api/v1/`

Nothing standard answers "what is in this registry", "what did this tag point at
last week", or "what is actually being pulled". These do. They are read-only
apart from `DELETE /api/v1/repositories/<name>`.

| Endpoint | Methods | Answers |
|---|---|---|
| `/api/v1/repositories` | `GET`, `HEAD` | Every repository, with tag and manifest counts. `?q=` filters by substring |
| `/api/v1/repositories/<name>` | `GET`, `HEAD` | One repository: tag, manifest and blob counts, and total size |
| `/api/v1/repositories/<name>` | `DELETE` | Delete a repository and everything in it |
| `/api/v1/tags/<name>` | `GET`, `HEAD` | Tags with the manifest each resolves to, inline |
| `/api/v1/manifests/<name>` | `GET`, `HEAD` | Manifests in digest order, with the tags pointing at each |
| `/api/v1/manifests/<name>@<reference>` | `GET`, `HEAD` | One manifest, by tag or by digest |
| `/api/v1/tag-history/<name>@<reference>` | `GET`, `HEAD` | Tag events, newest first |
| `/api/v1/pull-counts/<name>` | `GET`, `HEAD` | Pull counts for a repository, per day and per hour |
| `/api/v1/pull-counts/<name>@<reference>` | `GET`, `HEAD` | The same for one tag or one manifest |

Four conventions hold across all of them:

- **The route table is flat, and the name runs to the end of the path.** A
  nested `/repositories/<name>/tags` would be ambiguous in a registry holding
  both `foo` and `foo/tags`. A single manifest is `<name>@<reference>`, split at
  the last `@`, which appears in neither the name grammar, the tag grammar, nor
  a digest.
- **A reference is a tag or a digest, distinguished by a `:`,** the same rule
  `/v2/` uses. For history and pull counts the two are different questions, not
  two views of one answer.
- **The cursor is in the body,** as `next`, not in a `Link` header: a JSON
  caller has already parsed the body. `next` is `null` when the listing is
  exhausted, decided by peeking one key past the page. Pass it back as `?last=`.
  `?n=` defaults to 25 and is clamped to the range 1–100 — a row here costs a
  bounded count per repository, so the pages are far smaller than `/v2/`'s.
  `?n=0` has no special meaning here as it does on `/v2/`: it clamps to 1,
  because a zero-row page with a cursor is a client that never advances. A
  malformed `?n=` is still a `400`.
- **A count may be a floor.** Every count is an object `{"count": N,
  "complete": true|false}`. A `false` means the scan stopped at its ceiling and
  `N` is a lower bound — render it as `10,000+`. There is no stored total,
  because maintaining one would put a read-modify-write on the push path.

### Repositories

```
GET /api/v1/repositories?q=nginx&n=25&last=library/mysql
```

```json
{
  "repositories": [
    {"name": "library/nginx", "tags": {"count": 12, "complete": true},
     "manifests": {"count": 30, "complete": true}}
  ],
  "next": "library/nginx"
}
```

`?q=` matches anywhere in the name, not just the prefix, and is lowercased
before matching because a repository name cannot contain an uppercase byte. A
substring match cannot ride the key order, so the scan is bounded — which is why
`next` comes from the scan position and not from the last row returned.

### One repository

```
GET /api/v1/repositories/library/nginx
```

```json
{
  "name": "library/nginx",
  "tags": {"count": 12, "complete": true},
  "manifests": {"count": 30, "complete": true},
  "blobs": {"count": 214, "complete": true},
  "size_bytes": 4831838208
}
```

`size_bytes` is summed over the blobs counted, so it is a floor whenever
`blobs.complete` is `false`.

### Delete a repository

```
DELETE /api/v1/repositories/library/nginx
→ 202 Accepted
```

The one mutating route on this API, and it lives here because `/v2/` has no
spelling for it. Everything with a spec-defined meaning stays on `/v2/`, so
there is never a second set of rules to keep in agreement with the first.

`202` means what it says: the repository is gone from every listing by the time
the response returns — a `GET` of it, its tags or its manifests is an immediate
`404` — while the keys underneath are swept in the background. Nothing a client
can observe distinguishes the two states.

Blob bytes are not reclaimed by the delete. Layers are shared registry-wide, and
whether this repository was the last user of one is purge's question.

### Tags

```
GET /api/v1/tags/library/nginx?n=25
```

```json
{
  "tags": [
    {"name": "1.27", "digest": "sha256:…", "tagged_at": 1788696000,
     "manifest": {"digest": "sha256:…", "media_type": "application/vnd.oci.image.index.v1+json",
                  "size": 1206, "blob_size": 71303168, "artifact_type": null,
                  "subject": null, "pushed_at": 1788696000,
                  "platforms": ["linux/amd64", "linux/arm64"],
                  "blobs": 8, "children": 2, "tags": ["1.27", "latest"],
                  "annotations": {}}}
  ],
  "next": null
}
```

The manifest is inlined so a tag list is one request rather than one plus a
lookup per row. `manifest` is `null` only if the tag's target is missing.

### Manifests

```
GET /api/v1/manifests/library/nginx?n=25&last=sha256:…
GET /api/v1/manifests/library/nginx@1.27
GET /api/v1/manifests/library/nginx@sha256:…
```

The collection is digest-ordered, so its cursor is a digest, validated under the
same grammar a `/v2/` path segment gets — a cursor that does not parse is a
`400`, not a silent restart from the top. Rows have the shape of `manifest`
above; `tags` lists every tag pointing at that manifest, and an empty `tags` is
what makes a manifest purgeable.

### Tag history

```
GET /api/v1/tag-history/library/nginx@1.27
GET /api/v1/tag-history/library/nginx@sha256:…?before=1788696000000&last=sha256:…
```

```json
{
  "events": [
    {"at": 1788696000000, "tag": "1.27", "digest": "sha256:…", "event": "created",
     "media_type": "application/vnd.oci.image.index.v1+json", "size": 1206}
  ],
  "next": {"before": 1788696000000, "last": "sha256:…"}
}
```

A tag and a digest ask different questions: a tag asks *what has this name
pointed at*, a digest asks *what has this manifest been called*. They are served
by two indexes over the same events.

`event` is `created` or `deleted`. `at` is unix **milliseconds** — unlike
`tagged_at` and `pushed_at` next door, which are seconds — because two events on
one tag inside a second would otherwise collide. `media_type` and `size` are the
manifest's *at the time of the event*, kept in the event itself so a row still
renders after the manifest is gone.

`?before=` on its own is a filter — "what did this look like last Tuesday". With
`?last=` it is the exact resume `next` hands back; both halves are needed
because a page can end in the middle of an instant shared by several events.

A reference is mandatory here. There is no whole-repository history collection:
a fold across every tag's events is exactly the unbounded read this API does not
offer. Unknown repositories, tags and manifests return an empty page rather than
a `404`, because history outlives what it describes.

### Pull counts

```
GET /api/v1/pull-counts/library/nginx?days=30
GET /api/v1/pull-counts/library/nginx@1.27
GET /api/v1/pull-counts/library/nginx@sha256:…
```

```json
{
  "repository": "library/nginx",
  "reference": "1.27",
  "scope": "tag",
  "approximate": true,
  "from": "2026-08-08",
  "to": "2026-09-06",
  "totals": {"manifest_pulls": 1841, "blob_pulls": 9022, "bytes_out": 51539607552},
  "days": [
    {"day": 20673, "date": "2026-08-08", "weekday": 6,
     "manifest_pulls": 61, "blob_pulls": 300, "bytes_out": 1717986918,
     "hours": {"manifest_pulls": [0, 0, 4, 11, …],
               "blob_pulls": [0, 0, 20, 55, …],
               "bytes_out": [0, 0, 114294784, 314572800, …]}}
  ]
}
```

Three scopes — `repository`, `tag`, `manifest` — maintained as separate series
on write, not rolled up from each other. Only the repository scope carries blob
traffic. A tag and a digest again ask different questions: *how often is this
name pulled* against *how often is this content pulled*.

`?days=` sets the window's length, default 30 and clamped to 400; the window
always ends today. There is no cursor, because the window is the bound. Every
day in it is present whether or not it saw traffic, so a client rendering a grid
never has to fill a gap.

Days are UTC buckets fixed at write time and **must not be re-bucketed** — the
same wall would change shape depending on who was looking at it — but the hourly
arrays can be re-summed into any zone, and they answer "when in the day does
this get pulled" from the same response. `day` is the raw bucket (days since the
epoch), `date` is that day as `YYYY-MM-DD`, and `weekday` is `0` for Sunday, so
a contribution grid needs no calendar library. A day figure is the sum of its
hours; no total is stored.

`approximate` is always `true`, and it is sent precisely because it never
varies: increments are held in memory between flushes, so a crash loses up to
one interval. These are a popularity signal, not billing data. `--no-pull-counts`
stops recording; the API keeps serving what was already recorded.

Nothing here `404`s. An unknown repository, tag or manifest is a window of
zeroes — counts outlive what they describe.

## Error codes

| Code | Status | Sent when |
|---|---|---|
| `BLOB_UNKNOWN` | 404 | The blob is not in this repository |
| `BLOB_UPLOAD_INVALID` | 400, 416 | A bad upload request; `416` for an out-of-order chunk |
| `BLOB_UPLOAD_UNKNOWN` | 404 | No such upload session |
| `DIGEST_INVALID` | 400 | A malformed digest, or one that does not match the bytes |
| `MANIFEST_BLOB_UNKNOWN` | 400 | The manifest references a blob that was never pushed |
| `MANIFEST_INVALID` | 400, 413, 414 | Unparseable manifest; `413` over the size limit, `414` over the `?tag=` limit |
| `MANIFEST_UNKNOWN` | 404 | No manifest under that tag or digest |
| `NAME_INVALID` | 400 | The repository name violates the grammar |
| `NAME_UNKNOWN` | 404 | No such repository |
| `SIZE_INVALID` | 400, 413 | The uploaded length disagrees with what was declared; `413` over `--max-upload-bytes` |
| `UNAUTHORIZED` | 401 | Authentication required or the key is wrong |
| `DENIED` | 403 | Authenticated, but this key may not do this |
| `UNSUPPORTED` | 405 | The method is not allowed on this endpoint |
| `PAGINATION_NUMBER_INVALID` | 400 | A malformed `?n=`. Outside the spec's set — see above |

`TOOMANYREQUESTS` exists in the taxonomy and summ never sends it.
