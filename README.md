# summ Container Registry

[![CI](https://github.com/summcr/summ/actions/workflows/ci.yml/badge.svg)](https://github.com/summcr/summ/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**summ** is a simple yet powerful container registry with batteries included. It
fully supports the OCI Distribution Spec and adds the practical things a
registry should have had all along — image pull statistics, tag history, a
built-in web UI — so it is useful the moment it starts.

summ is written in Rust, with a bespoke data structure for extremely efficient
storage and retrieval of registry metadata, which is what makes it fast where
it counts: on the four serial metadata lookups a real `docker pull` waits on.

## Features

**Pull counts, per day and per hour.** Every repository, tag and manifest gets a
thirty-day contribution grid and a last-24-hours strip, so "what is anyone
actually pulling" is a page rather than a log-parsing exercise. Serving a pull
never touches the store — a `GET` adds to a map in memory and a background task
folds it into the metadata store every few seconds — so the counters cost the
pull path nothing.

**Tag history.** Every tag mutation has been recorded since the first push, so
you can ask what a tag has pointed at over time *and* what a manifest has ever
been called. Both are the same endpoint, addressed by tag or by digest, newest
first, cursor-paged. History outlives what it describes: a deleted tag still
answers, because "gone" is exactly the question you are asking.

**A built-in web UI.** Same binary, same port, assets compiled in — no build
step, no framework, no CDN, so it works air-gapped. Browse repositories with
per-repo tag and manifest counts, search names by substring, drill into a manifest,
and see the pull-count grids and tag timelines beside the thing they describe.

**Metadata lookups are the product.** Four of the five serial steps in a cold
`containerd` pull are metadata lookups, and their latencies add — so summ is
built around a purpose-designed key schema over RocksDB rather than around the
byte path. Nothing is a directory walk, no stored value grows with the size of
the registry, and prefix bloom filters make the hot existence checks ~6× faster
than the defaults. Measured: 7.42 GiB layers pushed at ~1.0 GB/s and pulled back
from four concurrent clients at ~1.1 GB/s aggregate.

**Discovery as a first-class API.** `/api/v1/` serves repositories, tags,
manifests, tag history and pull counts as a flat, cursor-paged, read-only
surface. Every list takes a cursor and a limit; the design target is 10M
repositories and up to 10M manifests in a single one, so nothing here
materialises an unbounded set. Both surfaces are documented in
[docs/api.md](docs/api.md).

**One binary, no dependencies.** RocksDB is compiled in and statically linked.
No database to run, no object store, no sidecar — `./summ serve` is the whole
deployment. Optional API-key auth (`--auth-mode open|public-pull|private`) puts
a read key and a write key in front of the registry, the discovery API and the
UI at once.

**Conformant.** The OCI `distribution-spec` conformance suite passes with zero
failures at every profile, including the OCI 1.1 referrers API — 1032 checks
passing at the suite's `dev` profile, with nothing skipped.

## Use cases

**Run your own registry, instead of pulling against someone else's limit.**
Docker Hub rate-limits anonymous and free-tier pulls; ECR and ACR throttle by
tier and bill the bandwidth on the way out. Meanwhile a scaling cluster and a CI
matrix fetch the same few base images hundreds of times a day. Copy them into
summ once — `skopeo copy`, `crane copy`, or a job that runs on merge — and the
pulls land on a registry you run, at your network's speed, with no quota to
exhaust.

**A throwaway registry for integration tests and CI.** One binary and a
directory — `summ serve --data-dir "$(mktemp -d)"` — no daemon, no compose file,
no service container to wait on. Bind it to loopback and Docker pushes to it
without an `insecure-registries` entry; delete the directory when the run ends.
Tests assert on what was pushed through the discovery API rather than by
grepping output.

**A personal registry that is not a weekend of YAML.** A homelab, a NAS, a
laptop, a few side projects that need somewhere to put images. `./summ serve` is
the whole deployment — no database, no object store, and the web UI is already
on the same port. Add `--auth-mode public-pull` when it leaves the laptop.

**A home for OCI artifacts that are not images.** Helm charts, WASM modules,
SBOMs, signatures, attestations, model weights — push them with `oras push` or
`helm push` like any other registry. The referrers API is implemented and passes
conformance, `artifactType` filtering included, so whatever is attached to an
image is discoverable by the tools that go looking.

**An air-gapped, edge or embedded registry.** One statically linked file, no
runtime dependencies, and a UI compiled into the binary that loads nothing from
a CDN — a machine with no route to the internet gets exactly the same registry
as one on the public network. Small enough to ship inside a product, an
appliance or a cluster bootstrap.

**Finding out what your registry is actually for.** Which repositories anyone
still pulls, when a tag last moved and what it pointed at before, whether a
manifest has ever been called anything else — questions most registries cannot
answer at all. summ answers them on the page you were already looking at.

## Quick start

Run the binary, or run the container. Either way you get a complete registry on
`http://127.0.0.1:3110` — nothing else to install, configure or stand up
alongside it.

### Prebuilt binary

Download the one for your platform:

```sh
# Linux x86_64
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-x86_64-unknown-linux-gnu.tar.gz | tar -xz summ

# Linux arm64
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-aarch64-unknown-linux-gnu.tar.gz | tar -xz summ

# macOS Apple silicon
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-aarch64-apple-darwin.tar.gz | tar -xz summ

# macOS Intel
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-x86_64-apple-darwin.tar.gz | tar -xz summ
```

Then start it:

```sh
./summ serve
```

Data goes in `./data` next to the binary. `--data-dir` puts it elsewhere.

### Docker

```sh
docker run -d --name summ -p 3110:3110 -v summ-data:/var/lib/summ summcr/summ
```

The image is multi-architecture, so that line is the same on x86_64 and arm64.

summ writes everything to `/var/lib/summ`, which `-v summ-data:/var/lib/summ`
keeps on a named volume so it survives the container. **Name the
volume** — drop the `-v` and you still get one, but an anonymous volume that
`docker run --rm` deletes along with the container. A bind mount works too,
after `chown 10001:10001` on the host directory.

### Check it works

```sh
curl http://127.0.0.1:3110/v2/     # {}
```

Then open <http://127.0.0.1:3110> for the web UI, and push an image at it:

```sh
docker tag alpine 127.0.0.1:3110/demo/alpine
docker push 127.0.0.1:3110/demo/alpine
```

Docker treats the whole `127.0.0.0/8` range as insecure by default, so there is
nothing to configure. Other clients make their own rules — `oras` and `crane`
have to be told an endpoint is plain HTTP.

One caveat on macOS and Windows: that push reaches the container above, but not
a *binary* on your host — the Docker daemon runs in a VM there, where
`127.0.0.1` is its own loopback rather than yours. Push to a host binary with
`oras` or `crane` instead.

More in [docs/setup.md](docs/setup.md), including platform requirements and
building from source, and [docs/data-dir.md](docs/data-dir.md) for what summ
stores and how to back it up.

## Deployment

`./summ serve` is already a complete registry, so there is nothing to stand up
alongside it. For running it as a long-lived service — a dedicated user, a
sandboxed systemd unit, and TLS terminated in front of it — see
[DEPLOYMENT.md](DEPLOYMENT.md).
