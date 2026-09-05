# summ documentation

summ is a single-binary OCI container registry. It passes the OCI Distribution
Spec conformance suite, keeps its metadata in an embedded RocksDB store, and
ships a discovery API and a web UI in the same process.

It is a fit when you want a registry that:

- runs inside your own network with nothing else to stand up: no database, no
  object store, no token server;
- starts in one command and is fast on the metadata lookups a `docker pull`
  actually waits on;
- answers "what is being pulled" and "what did this tag point at" without
  log parsing.

| Document | What it covers |
|---|---|
| [Setup](setup.md) | Install, first run, pushing an image, running as a service or container |
| [Data directory](data-dir.md) | What lives under `--data-dir`, backup, and the one-filesystem rule |
| [Authentication](auth.md) | The three `--auth-mode` values, keys, and client login |
| [Architecture](architecture.md) | Crates, the RocksDB metadata engine, the blob store, and the write ordering |

Every flag has an environment-variable twin. `summ serve --help` lists both.
