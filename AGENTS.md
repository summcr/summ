# summ for agents

summ is an OCI Distribution Spec container registry in one binary: no database,
no object store, no token server. It serves `/v2/` for registry clients,
`/api/v1/` for discovery, and a web UI, all on one port — `127.0.0.1:3110` by
default.

This file is the operating manual for an automated caller. Everything in it has
been run as written.

## Start one, without blocking

`summ serve` runs in the foreground and never returns, so background it and wait
on the port rather than on the process:

```sh
curl -fsSL https://summcr.com/install.sh | sh
./summ serve > summ.log 2>&1 &
for _ in $(seq 100); do curl -fsS http://127.0.0.1:3110/v2/ >/dev/null 2>&1 && break; sleep 0.1; done
```

`GET /v2/` answering `200` with `{}` is the readiness signal, and the same check
is the right liveness probe for an orchestrator. Under `--auth-mode private` it
answers `401` until a key is presented, which is also a started registry.

Data goes in `./data`; `--data-dir` puts it elsewhere. Every flag has an
environment twin (`SUMM_LISTEN`, `SUMM_DATA_DIR`, …) and `./summ serve --help`
lists both.

## Let the kernel choose the port

Port 3110 may be taken, and a fixed port makes two concurrent jobs collide.
`--listen 127.0.0.1:0` binds an ephemeral port, and summ prints the one it
actually got — it reads the address back from the listener rather than echoing
the argument:

```sh
./summ serve --listen 127.0.0.1:0 --data-dir "$(mktemp -d)" > summ.log 2>&1 &
for _ in $(seq 100); do
  addr=$(awk '/listening on/ {print $3; exit}' summ.log)
  [ -n "$addr" ] && break
  sleep 0.1
done
curl -fsS "http://$addr/v2/"
```

The banner goes to stdout and is printed, not logged, so it survives any
`SUMM_LOG` setting. The line is `  listening on  127.0.0.1:55803`.

That plus a `mktemp -d` data directory is the whole recipe for a throwaway
registry in a test: nothing to install, no daemon, no compose file, and the
teardown is killing the process and deleting the directory.

## Push something at it

Loopback is plain HTTP. `docker` treats all of `127.0.0.0/8` as insecure and
needs no configuration; every other client has to be told, with `--plain-http`
or its equivalent:

```sh
oras push --plain-http "$addr/demo/hello:v1" hello.txt
```

On macOS and Windows a `docker push` reaches a summ running *in a container*
with a published port, but not a summ binary on the host — the daemon is in a
VM there, so its `127.0.0.1` is not yours. Use `oras`, `crane` or `skopeo` on
the host instead.

## Ask it what it holds

`/api/v1/` is summ's own API, and it is where an agent should look — `/v2/` is
a transfer protocol and cannot answer most of these questions. It is flat: each
collection is a top-level resource with the repository name running to the end
of the path, and a single item is `<name>@<reference>`, split at the last `@`.

```
GET /api/v1/repositories?q=<substring>&n=<limit>&last=<cursor>
GET /api/v1/repositories/<name>
GET /api/v1/tags/<name>
GET /api/v1/manifests/<name>            GET /api/v1/manifests/<name>@<ref>
GET /api/v1/tag-history/<name>@<ref>
GET /api/v1/pull-counts/<name>          GET /api/v1/pull-counts/<name>@<ref>
GET /api/v1/purge
```

Four things to hold on to, because they change how a response is read:

- **Every list is cursor-paged and nothing is unbounded.** A response carries
  `next`; only a `null` `next` ends a listing. A page may come back short, or
  empty, with `next` still set — `?q=` filters inside a bounded scan — so never
  stop on a short page.
- **Counts are `{"count": N, "complete": true|false}`.** A `false` means the
  scan hit its ceiling and `N` is a floor. There is no stored total.
- **Timestamps are unix seconds, except tag history, which is milliseconds.**
- **Tag history and pull counts never `404`.** An unknown repository, tag or
  manifest is an empty page or a window of zeroes, because both outlive what
  they describe. The `/v2/` and repository routes do `404` normally.

Errors on both surfaces use the spec envelope, so failures are machine-readable
without parsing prose:

```json
{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known to registry","detail":"demo/app"}]}
```

Full request and response shapes, every field, and the error-code table are in
[docs/api.md](https://github.com/summcr/summ/blob/main/docs/api.md).

## Credentials

`--auth-mode open` (the default) needs none. `public-pull` needs the write key
to push, `private` needs a key for everything including the UI. Keys are API
keys sent as an HTTP Basic password — the username is ignored — so
`docker login -u anyone -p "$KEY"` works with no token server. `curl` may send
`Authorization: Bearer <key>` instead.

An omitted key is generated and printed once in the startup banner. A key passed
to a mode that does not need it is a startup error, not a warning. Details in
[docs/auth.md](https://github.com/summcr/summ/blob/main/docs/auth.md).

## What not to do unprompted

Three routes destroy data, and none of them asks twice:

- `DELETE /api/v1/repositories/<name>` — releases the name immediately and
  sweeps the keys behind it. There is no undo.
- `DELETE /v2/<name>/manifests/<reference>` — deletes a manifest, and its tags
  with it.
- `POST /api/v1/purge` — runs a reclamation pass now. Use
  `POST /api/v1/purge?dry-run=true` to find out what a pass would do; it counts
  everything and writes nothing, marks included.

Purge also runs on a schedule (`--purge-interval`, hourly), so a registry
reclaims its own space and an agent never needs to call it to keep disk in
check. Blob bytes are not reclaimed by a repository delete: layers are shared
registry-wide, so that is purge's question.

A first purge pass over a store reports many blobs `marked` and none
reclaimed. That is the grace period's clock starting, not a failure — the bytes
come back a grace period later.

## Where the rest is

| Document | Covers |
|---|---|
| [docs/api.md](https://github.com/summcr/summ/blob/main/docs/api.md) | Both HTTP surfaces, field by field, with the error codes |
| [docs/setup.md](https://github.com/summcr/summ/blob/main/docs/setup.md) | Install, platform requirements, flags, container, building from source |
| [docs/auth.md](https://github.com/summcr/summ/blob/main/docs/auth.md) | The three auth modes, keys, and client login |
| [docs/data-dir.md](https://github.com/summcr/summ/blob/main/docs/data-dir.md) | What is on disk, backup, and the one-filesystem rule |
| [docs/architecture.md](https://github.com/summcr/summ/blob/main/docs/architecture.md) | Crates, the metadata engine, the blob store, write ordering |
| [DEPLOYMENT.md](https://github.com/summcr/summ/blob/main/DEPLOYMENT.md) | Running it as a long-lived service, behind TLS |
