# Setup

## Install

Prebuilt binaries are on the
[releases page](https://github.com/summcr/summ/releases). The asset names carry
no version, so this line never changes:

```sh
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz summ
```

| Platform | Asset |
|---|---|
| Linux x86_64 | `summ-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `summ-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `summ-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `summ-x86_64-apple-darwin.tar.gz` |

Linux builds need glibc 2.34 or newer (Ubuntu 22.04, Debian 12, RHEL 9).
RocksDB and its C++ runtime are linked in statically, so the only shared
libraries left are ones every glibc system already has. The macOS builds link
nothing beyond the OS's own libraries and run on macOS 11 and up.

Every asset has a `.sha256` beside it. `dev` is a rolling prerelease built from
`main` on demand — swap `latest/download` for `download/dev` to fetch it. Being
a prerelease it never becomes `latest`.

To build from source you need a Rust toolchain, a C++ compiler, and `clang`
plus `lld` on Linux. RocksDB compiles from source, so the first build takes a
few minutes.

```sh
cargo build --release --locked --bin summ
```

## First run

```sh
./summ serve
```

That is a complete registry. The banner tells you where it is:

```
summ 0.1.0
  listening on  127.0.0.1:3110
  registry      http://127.0.0.1:3110/v2/
  data dir      /home/you/data
  auth mode     open - no credential is required, to pull or to push
```

Open `http://127.0.0.1:3110/` in a browser for the UI. Push something:

```sh
docker tag alpine:3.20 127.0.0.1:3110/demo/alpine:3.20
docker push 127.0.0.1:3110/demo/alpine:3.20
```

Docker treats the whole `127.0.0.0/8` range as insecure by default — `::1/128`
too, on current daemons — so no daemon configuration is needed for a local
test. The behaviour has held since Docker 1.3.2, though the `dockerd` reference
discourages relying on it. Any other address needs TLS in front of summ, or an
`insecure-registries` entry in the daemon config.

That is a Docker rule and not a general one. `oras`, `crane` and `skopeo` each
need to be told an endpoint is plain HTTP — `--plain-http` or the equivalent —
loopback included.

On macOS and Windows this works only when summ is itself a container with a
published port. The Docker daemon runs in a VM there, so the `127.0.0.1` it
dials is the VM's loopback and not the host's, and a push to a summ binary
running on your machine fails with `connection refused`. Use a client that runs
on the host — `oras`, `crane` or `skopeo` — or run summ in a container. On
Linux the daemon shares your network namespace and the question does not arise.

## The flags you will set

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--listen` | `SUMM_LISTEN` | `127.0.0.1:3110` | IP and port. Use `0.0.0.0:3110` to serve the network. |
| `--data-dir` | `SUMM_DATA_DIR` | `./data` | Where everything is stored. See [Data directory](data-dir.md). |
| `--auth-mode` | `SUMM_AUTH_MODE` | `open` | `open`, `public-pull`, or `private`. See [Authentication](auth.md). |
| `--max-upload-bytes` | `SUMM_MAX_UPLOAD_BYTES` | 32 GiB | Largest layer accepted. `0` removes the limit. |
| `--no-pull-counts` | `SUMM_NO_PULL_COUNTS` | off | Stop recording pull statistics. |

Logging is controlled by `SUMM_LOG`, using `tracing` filter syntax. The default
is `summ=info,summ_server=info,tower_http=info`.

A typical in-network deployment:

```sh
summ serve --listen 0.0.0.0:3110 --data-dir /var/lib/summ --auth-mode public-pull
```

Binding to a non-loopback address in `open` mode prints a boxed warning,
because it means anyone on the network can push and delete.

## Run as a service

summ speaks plain HTTP and expects a reverse proxy to terminate TLS. The full
procedure for a dedicated user, a sandboxed systemd unit, and a Caddy example
is in [DEPLOYMENT.md](../DEPLOYMENT.md) at the repository root.

Two proxy rules matter more than the rest:

- Do not cap request body size below your largest layer. Clients send a layer
  as one body, and a cap becomes the largest image you can push.
- Do not buffer request bodies. summ streams uploads to disk, and a buffering
  proxy adds back the memory cost that design removes.

## Run in a container

Multi-architecture images for `linux/amd64` and `linux/arm64` are published on
Docker Hub as `summcr/summ`. They listen on `0.0.0.0:3110` and store data at
`/var/lib/summ` as uid 10001.

```sh
docker run -d --name summ -p 3110:3110 -v summ-data:/var/lib/summ summcr/summ:0.1.0-rc.1
```

`latest` points at the newest release, release candidates included. Pin a
version for anything that should stay put.

Mount `/var/lib/summ` as one volume rather than one per subdirectory — `meta/`,
`blobs/` and `uploads/` must share a filesystem, for the reason in
[Data directory](data-dir.md). The image declares `VOLUME`, so a run with no
`-v` gets an anonymous volume, and `docker run --rm` deletes that with the
container.

Pass `serve` flags after the image name, or set the `SUMM_*` variables with
`-e`. The image has a healthcheck on `GET /v2/`, which is also the right
liveness probe for any orchestrator.

To build an image from an unreleased commit instead, the repository
`Dockerfile` compiles from source; `Dockerfile.release` is what packages a
published release.

## Check it is up

```sh
curl -fsS http://127.0.0.1:3110/v2/
```

A `200` with an empty JSON object means the registry is serving. Under
`private` mode this returns `401` until a key is presented, which is the
expected answer.
