# Data directory

`--data-dir` (or `SUMM_DATA_DIR`) names the one directory summ writes to. It
defaults to `./data` relative to the working directory and is created on first
start. The container image sets it to `/var/lib/summ`.

## Layout

```
<data-dir>/
  meta/                        RocksDB metadata store
  blobs/<algo>/ab/cd/ef/<hex>  content-addressed blobs; the file is the blob
  uploads/<id>                 staging files for in-progress uploads
```

**`meta/`** holds everything that is not blob bytes: repositories, tags,
manifest records and bodies, reference edges, tag history, pull counters, and
the schema version. Manifests live here, not in `blobs/`.

**`blobs/`** is a content-addressed tree. The path is derived from the digest
and nothing else. No directory name carries a relationship, and summ never
lists a directory to answer a question. Three fan-out levels keep directories
small at hundreds of millions of blobs.

**`uploads/`** holds a layer while it is arriving. On completion the staging
file is renamed into `blobs/`. Files left here belong to abandoned uploads.

## One filesystem

`meta/`, `blobs/` and `uploads/` must be on the same filesystem. An upload is
committed by renaming its staging file into the blob tree, and a rename across
devices is a copy, which breaks the durability ordering described in
[Architecture](architecture.md).

If the data directory is its own mount, make the service depend on that mount.
Otherwise a failed mount leaves summ starting with an empty store on the root
filesystem, serving an empty catalog and accepting writes to the wrong disk.
The systemd unit in `DEPLOYMENT.md` uses `RequiresMountsFor` for this.

## Ownership and limits

Run summ as a dedicated user that can write to this directory and nothing else.
The Dockerfile and `DEPLOYMENT.md` both use uid and gid 10001, so a directory
can move between a host service and a container without a chown.

RocksDB keeps many SST files open and every in-flight pull holds a blob file
descriptor. Raise the open-file limit to at least 65535.

## Backup

The metadata store is the registry. Manifest bodies and tags exist only under
`meta/`, so a copy of `blobs/` alone cannot be rebuilt into a registry.

Stop the process, or take a filesystem snapshot, before copying. A live copy of
`meta/` can capture RocksDB mid-write. Copy `meta/` and `blobs/` together;
`uploads/` can be skipped.

## Schema version

`meta/` carries a version marker. A build that does not understand the stored
version refuses to start with a message rather than opening the store and
returning undecodable records. Migrations, when they exist, run at open and are
safe to re-run after an interrupted start.

A `meta/summ.redb` file comes from a pre-release build's verification engine.
summ refuses to open RocksDB beside it, because doing so would start an empty
registry next to your real metadata. Move the file aside to start fresh.
