# Authentication

summ has one auth axis with three settings. `--auth-mode` (or
`SUMM_AUTH_MODE`) says how open the registry is. Keys, when needed, are API
keys passed as the password of an HTTP Basic credential.

| Mode | Pull, list, browse UI | Push, delete | Keys |
|---|---|---|---|
| `open` (default) | anyone | anyone | none accepted |
| `public-pull` | anyone | write key | `--write-apikey` |
| `private` | read key or write key | write key | `--read-apikey`, `--write-apikey` |

The mode applies to everything the process serves: `/v2/`, the discovery API
under `/api/v1/`, and the UI. There is no exemption list. Read means `GET`,
`HEAD` and `OPTIONS`; every other method is a write.

## open

No credential is required, and none is accepted. Supplying a key in this mode
is a startup error, so a stray `SUMM_WRITE_APIKEY` cannot leave you believing
the registry is locked when it is not.

On loopback this is a laptop registry. On any other address the banner prints
a boxed warning, because the whole network can push and delete.

## public-pull

The shape of a public registry: anonymous pull, authenticated push. The
catalog, every tag, every manifest and the UI are readable by anyone who can
reach the port.

```sh
summ serve --auth-mode public-pull --write-apikey "$KEY"
```

Omit the key and summ generates one, printing it once at startup. A key you
supply is never echoed. A `--read-apikey` in this mode is a startup error,
since reads need no key.

## private

A key for every request. The read key admits reads. The write key admits
everything, so a client that both pushes and pulls needs only the write key.

```sh
summ serve --auth-mode private --read-apikey "$READ" --write-apikey "$WRITE"
```

Either key, when absent, is generated and printed once. Generated keys are not
stored anywhere; if you lose one, restart with a key of your own.

## Keys

A generated key is 32 random bytes as lowercase hex. There is no format
requirement on a supplied key, and a key never appears in logs or in a debug
dump of the configuration.

A supplied credential is checked in every mode, even on a request that needed
none. A wrong key on an anonymous read gets `401`, so a misconfigured client
finds out on its first request rather than on its first push.

## Client login

summ challenges with `Basic`, not `Bearer`. There is no token server, and the
key is the credential itself. Every standard client sends its stored
credentials in reply:

```sh
docker login 127.0.0.1:3110 -u anyone -p "$KEY"
oras login 127.0.0.1:3110 -u anyone -p "$KEY"
podman login 127.0.0.1:3110 -u anyone -p "$KEY"
```

The username is ignored. A browser opening the UI gets its native password
prompt; leave the username blank or type anything.

`Authorization: Bearer <key>` is also accepted, for `curl`:

```sh
curl -H "Authorization: Bearer $KEY" http://127.0.0.1:3110/v2/_catalog
```

One caveat under `public-pull`: `docker login` succeeds with any key. The
client pings `GET /v2/` without a credential, gets `200`, and never sends the
key to be checked. The push is the first request that validates it. Under
`private` the ping returns `401` and a wrong key fails at login.

## Environment file

For a service, keep the mode and keys in a root-only file and load it with
`EnvironmentFile=` in systemd or `--env-file` in Docker:

```sh
umask 077
cat > /etc/summ.env <<EOF
SUMM_AUTH_MODE=public-pull
SUMM_WRITE_APIKEY=$(head -c 32 /dev/urandom | base64 | tr -d '=+/' | cut -c1-40)
EOF
```
