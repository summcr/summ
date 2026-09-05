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
RocksDB is statically linked, so there is nothing else to install.

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

`127.0.0.1` is on Docker's default insecure-registry list, so no daemon
configuration is needed for a local test. Any other address needs TLS in front
of summ, or an `insecure-registries` entry in the daemon config.

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

The repository `Dockerfile` builds a runtime image that listens on `0.0.0.0:3110`
and stores data at `/var/lib/summ` as uid 10001.

```sh
docker build -t summ .
docker run -d --name summ -p 3110:3110 -v summ-data:/var/lib/summ summ
```

Pass `serve` flags after the image name, or set the `SUMM_*` variables with
`-e`. The image has a healthcheck on `GET /v2/`, which is also the right
liveness probe for any orchestrator.

## Check it is up

```sh
curl -fsS http://127.0.0.1:3110/v2/
```

A `200` with an empty JSON object means the registry is serving. Under
`private` mode this returns `401` until a key is presented, which is the
expected answer.
