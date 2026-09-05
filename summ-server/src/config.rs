//! Server configuration and the CLI that builds it.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::auth::{AuthPolicy, Generated};

/// Default ceiling on one upload request body, in bytes.
///
/// 32 GiB: comfortably above the largest layer anyone actually pushes - the
/// CUDA layers in `pytorch/pytorch` are the usual worst case, at a few
/// gigabytes - while still bounding what one request can write to the disk.
/// `0` on the command line removes it entirely.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Limits and switches the handlers consult. Separate from [`Cli`] so tests can
/// construct one directly and so a future config file has somewhere to land.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Largest manifest accepted, in bytes. Above it, `413`.
    ///
    /// The spec asks for at least 4 MiB and the conformance suite pushes a
    /// 3.92 MB manifest (390 annotations of 10,000 characters), which is
    /// uncomfortably close to a 4 MiB cap. 8 MiB costs nothing - the body is
    /// compressed at rest - and moves the margin from 2 % to 100 %.
    pub max_manifest_bytes: usize,
    /// Largest single request body accepted on an upload `POST`/`PATCH`/`PUT`,
    /// or `None` for no ceiling at all.
    ///
    /// In principle a per-request bound rather than a blob-size bound, since a
    /// larger blob may be pushed in chunks. In practice **no client chunks** -
    /// docker, crane and oras all send a layer as one monolithic body - so this
    /// is the largest layer the registry accepts, and a low value rejects
    /// exactly the multi-gigabyte ML images a registry gets pushed.
    ///
    /// It was 1 GiB while the skeleton buffered a chunk in memory. The body
    /// streams into the staging file now and costs one frame whatever the blob,
    /// so what is left is a guard against a runaway or malicious client filling
    /// a disk - which wants a number far above any real layer, not one near it.
    pub max_upload_bytes: Option<u64>,
    /// Page size used when `?n=` is absent.
    pub default_page_size: usize,
    /// Ceiling for `?n=`. An oversized `n` is **clamped, not rejected**: the
    /// spec explicitly permits returning fewer than `n` results when a `Link`
    /// header is supplied, and rejecting is how the reference implementation
    /// makes a 10M-repo catalog unusable.
    pub max_page_size: usize,
    /// Maximum `?tag=` parameters on one manifest `PUT` (end-7b). The spec says
    /// a registry SHOULD accept at least 10 and MAY answer `414` above its own
    /// limit.
    pub max_tag_params: usize,
    /// Advertised `OCI-Chunk-Min-Length`, if any.
    ///
    /// Off by default. It is optional, and advertising a minimum makes the
    /// conformance suite size its test blobs to match, so claiming one we do
    /// not need only makes the suite push more bytes.
    pub chunk_min_length: Option<u64>,
    /// Whether `/v2/<name>/referrers/<digest>` is served.
    ///
    /// **On.** The `F` edges have been written since the first push, so the
    /// switch was only ever hiding a working endpoint. It stays a switch
    /// because turning the API off is not the same as never having had it: a
    /// client that gets a `404` here MUST fall back to the referrers tag
    /// schema, and that fallback is the only thing an operator can reach for
    /// if the edges ever need rebuilding.
    ///
    /// It also gates `OCI-Subject` on a manifest `PUT`. The spec ties the two
    /// together - the header means "this registry processed your subject", and
    /// a registry that sends it while answering `404` on `/referrers/` has told
    /// the client both that the fallback is unnecessary and that it is
    /// required.
    pub referrers_enabled: bool,
    /// How open the registry is, and which keys enforce it.
    ///
    /// [`AuthPolicy::Open`] by default, which is what makes `summ serve` with
    /// no arguments a working registry in one command - see [`crate::auth`].
    pub auth: AuthPolicy,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_manifest_bytes: 8 * 1024 * 1024,
            max_upload_bytes: Some(DEFAULT_MAX_UPLOAD_BYTES),
            default_page_size: 1000,
            max_page_size: 1000,
            max_tag_params: 32,
            chunk_min_length: None,
            referrers_enabled: true,
            auth: AuthPolicy::Open,
        }
    }
}

/// How open the registry is.
///
/// One axis, three points, from open to closed. The names describe the
/// registry's posture rather than the mechanism enforcing it, which leaves
/// room for a second mechanism to be a separate choice instead of a fourth
/// value here that silently also decides the scope.
///
/// `public-pull` names an operation where its neighbours name a posture, and
/// that asymmetry is the point: `open` and `private` are unambiguous alone,
/// while a bare `public` is not - a public repository on a hosted registry is
/// one anyone may *pull*, and summ's default is one anyone may also push.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthMode {
    /// Anonymous pull and push; no key is required, or accepted
    Open,
    /// Anonymous pull; the write key to push or delete
    PublicPull,
    /// A key for every request, the UI included
    Private,
}

#[derive(Debug, Parser)]
#[command(name = "summ", version, about = "An OCI Distribution Spec registry")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the registry.
    Serve(ServeArgs),
}

#[derive(Debug, Parser)]
pub struct ServeArgs {
    /// Address to listen on, as `<host>:<port>`.
    ///
    /// `127.0.0.1:3110` for this machine only, `0.0.0.0:3110` for every IPv4
    /// interface, `[::]:3110` for every interface on both families. Port `0`
    /// binds an ephemeral port, which the startup banner then reports. Takes
    /// an IP address, not a hostname.
    #[arg(long, default_value = "127.0.0.1:3110", env = "SUMM_LISTEN")]
    pub listen: SocketAddr,

    /// Directory for blobs and metadata.
    ///
    /// Created if absent. `meta/` holds the metadata store, `blobs/` the
    /// content-addressed blob tree and `uploads/` the ones still arriving; all
    /// three must be on one filesystem.
    #[arg(long, default_value = "./data", env = "SUMM_DATA_DIR")]
    pub data_dir: PathBuf,

    /// Accept a manifest whose layers or child manifests are not present yet.
    ///
    /// Off by default, so a manifest naming an absent blob is rejected with
    /// `400 MANIFEST_BLOB_UNKNOWN`. Set it to push a manifest and its blobs
    /// concurrently.
    #[arg(long, env = "SUMM_ALLOW_MISSING_REFERENCES")]
    pub allow_missing_references: bool,

    /// Maximum manifest size in bytes. A larger one is rejected with `413`.
    #[arg(long, default_value_t = 8 * 1024 * 1024, env = "SUMM_MAX_MANIFEST_BYTES")]
    pub max_manifest_bytes: usize,

    /// Maximum bytes in one upload request body; `0` removes the limit.
    ///
    /// Effectively the largest layer the registry accepts, since no client
    /// chunks one: a bigger body is rejected with `413 SIZE_INVALID`. The body
    /// streams to disk, so raising it costs disk rather than memory.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_UPLOAD_BYTES,
        env = "SUMM_MAX_UPLOAD_BYTES"
    )]
    pub max_upload_bytes: u64,

    /// Default number of results for a list endpoint with no `?n=`.
    #[arg(long, default_value_t = 1000, env = "SUMM_DEFAULT_PAGE_SIZE")]
    pub default_page_size: usize,

    /// Ceiling for `?n=`; larger requests are clamped to it.
    #[arg(long, default_value_t = 1000, env = "SUMM_MAX_PAGE_SIZE")]
    pub max_page_size: usize,

    /// Answer `404` on `/v2/<name>/referrers/<digest>` instead of serving it.
    ///
    /// The endpoint is on by default. Turning it off also drops `OCI-Subject`
    /// from manifest `PUT` responses.
    #[arg(long, env = "SUMM_NO_REFERRERS")]
    pub no_referrers: bool,

    /// Stop counting pulls.
    ///
    /// Counting is on by default. This stops the recording and its background
    /// writes, not the API: `/api/v1/pull-counts/` keeps answering with
    /// whatever was recorded before.
    #[arg(long, env = "SUMM_NO_PULL_COUNTS")]
    pub no_pull_counts: bool,

    /// How open the registry is: `open`, `public-pull` or `private`.
    ///
    /// `open`, the default, requires no credential to pull or to push.
    /// `public-pull` serves every read anonymously - the catalog and the UI
    /// included - and requires the write key to push or delete. `private`
    /// requires a key for everything the server serves, the UI included.
    ///
    /// A key is sent as the password of an HTTP Basic credential, so
    /// `docker login -u anyone -p <key>` works and the username is ignored.
    #[arg(long, value_enum, default_value = "open", env = "SUMM_AUTH_MODE")]
    pub auth_mode: AuthMode,

    /// API key admitting `GET` and `HEAD` - pull, list, browse.
    ///
    /// Valid only with `--auth-mode private`, where an absent key is generated
    /// and printed once at startup; supplying it in any other mode is a
    /// startup error. A key given here is never printed back.
    #[arg(long, env = "SUMM_READ_APIKEY")]
    pub read_apikey: Option<String>,

    /// API key admitting everything, reads included - push, delete.
    ///
    /// Required by `--auth-mode public-pull`, and the stronger of the two keys
    /// under `--auth-mode private`; absent, it is generated and printed once
    /// at startup. It reads as well as writes, so a client that both pushes
    /// and pulls needs this key alone.
    #[arg(long, env = "SUMM_WRITE_APIKEY")]
    pub write_apikey: Option<String>,
}

impl ServeArgs {
    /// The ops layer's own limits, which are not the HTTP layer's.
    ///
    /// `max_manifest_bytes` appears in both on purpose and is deliberately the
    /// same number: the handler's copy decides a `413` before the body is read,
    /// and the ops layer's is the backstop for any caller that did not come
    /// through HTTP.
    pub fn registry_options(&self) -> summ_registry::RegistryOptions {
        summ_registry::RegistryOptions {
            validate_references: !self.allow_missing_references,
            max_manifest_bytes: self.max_manifest_bytes,
            // The search ceiling is deliberately not a flag. It bounds how far
            // one request walks, not what a search can find, so tuning it trades
            // round trips against per-request latency - a property of the scan
            // rather than a decision a deployment has to make.
            ..Default::default()
        }
    }

    /// The HTTP layer's configuration, plus which API keys had to be invented.
    ///
    /// Fallible because the auth arguments can contradict each other, and the
    /// contradiction has to be fatal. A key supplied to a mode that does not
    /// require it is somebody who believes they have locked something; the
    /// alternatives to failing are to ignore the key, which serves a more open
    /// registry than its operator thinks, or to infer the mode from the key's
    /// presence, which makes deleting an environment variable silently
    /// downgrade authentication. Neither is a failure an operator can see, so
    /// this one is made loud and made at startup, before the listener binds.
    pub fn server_config(&self) -> Result<(ServerConfig, Option<Generated>), String> {
        let (auth, generated) = match self.auth_mode {
            AuthMode::Open => {
                if self.read_apikey.is_some() || self.write_apikey.is_some() {
                    return Err("an API key was supplied but --auth-mode is `open`; pass \
                         `--auth-mode public-pull` (SUMM_AUTH_MODE=public-pull) for \
                         anonymous pull and keyed push, or `--auth-mode private` to \
                         require a key for reads as well"
                        .to_owned());
                }
                (AuthPolicy::Open, None)
            }
            AuthMode::PublicPull => {
                if self.read_apikey.is_some() {
                    return Err("a read API key was supplied but --auth-mode is \
                         `public-pull`, which serves every read anonymously; pass \
                         `--auth-mode private` (SUMM_AUTH_MODE=private) to require a key \
                         for reads too"
                        .to_owned());
                }
                let (policy, generated) = AuthPolicy::for_public_pull(self.write_apikey.clone())?;
                (policy, Some(generated))
            }
            AuthMode::Private => {
                let (policy, generated) =
                    AuthPolicy::for_private(self.read_apikey.clone(), self.write_apikey.clone())?;
                (policy, Some(generated))
            }
        };
        let config = ServerConfig {
            max_manifest_bytes: self.max_manifest_bytes,
            // Zero is "no ceiling" rather than "accept nothing": a limit flag
            // is set to zero by someone turning the limit off, and the other
            // reading makes every push fail.
            max_upload_bytes: (self.max_upload_bytes > 0).then_some(self.max_upload_bytes),
            default_page_size: self.default_page_size,
            max_page_size: self.max_page_size,
            referrers_enabled: !self.no_referrers,
            auth,
            ..ServerConfig::default()
        };
        Ok((config, generated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `serve` argument list. The auth arguments are always passed
    /// explicitly, so a `SUMM_*` variable in the environment running the tests
    /// cannot change what is under test.
    fn args(extra: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(extra);
        ServeArgs::try_parse_from(argv)
    }

    #[test]
    fn the_default_is_an_open_registry() {
        assert!(!ServerConfig::default().auth.is_enabled());
        let args = args(&["--auth-mode", "open"]).expect("parses");
        let (config, generated) = args.server_config().expect("valid");
        assert!(!config.auth.is_enabled());
        assert!(generated.is_none(), "nothing to generate when auth is off");
    }

    #[test]
    fn a_key_without_the_mode_is_a_startup_error() {
        // The failure this prevents is silent: an operator who set
        // SUMM_READ_APIKEY and believes the registry is closed.
        let args = args(&["--auth-mode", "open", "--read-apikey", "k"]).expect("parses");
        let message = args.server_config().expect_err("must not start");
        assert!(message.contains("--auth-mode public-pull"), "{message}");
        assert!(message.contains("--auth-mode private"), "{message}");
    }

    #[test]
    fn public_pull_takes_one_key_and_refuses_the_other() {
        let supplied =
            args(&["--auth-mode", "public-pull", "--write-apikey", "mine"]).expect("parses");
        let (config, generated) = supplied.server_config().expect("valid");
        assert!(config.auth.is_enabled());
        let generated = generated.expect("reported");
        assert!(!generated.write, "the key was supplied");
        assert!(!generated.read, "this mode has no read key to invent");

        let bare = args(&["--auth-mode", "public-pull"]).expect("parses");
        let (_, generated) = bare.server_config().expect("valid");
        assert!(
            generated.expect("reported").write,
            "an absent write key is generated, or the mode locks the operator out"
        );

        // A read key here is an operator who believes reads are guarded.
        let contradictory =
            args(&["--auth-mode", "public-pull", "--read-apikey", "k"]).expect("parses");
        let message = contradictory.server_config().expect_err("must not start");
        assert!(message.contains("--auth-mode private"), "{message}");
    }

    #[test]
    fn private_generates_what_was_not_supplied() {
        let args = args(&["--auth-mode", "private", "--write-apikey", "mine"]).expect("parses");
        let (config, generated) = args.server_config().expect("valid");
        assert!(config.auth.is_enabled());
        let generated = generated.expect("reported");
        assert!(generated.read, "the read key had to be invented");
        assert!(!generated.write);

        let args = args_both();
        let (_, generated) = args.server_config().expect("valid");
        let generated = generated.expect("reported");
        assert!(!generated.read);
        assert!(!generated.write, "nothing to print when both were supplied");
    }

    fn args_both() -> ServeArgs {
        args(&[
            "--auth-mode",
            "private",
            "--read-apikey",
            "r",
            "--write-apikey",
            "w",
        ])
        .expect("parses")
    }

    #[test]
    fn an_empty_key_is_rejected_rather_than_matching_an_empty_credential() {
        let empty_read = args(&["--auth-mode", "private", "--read-apikey", " "]).expect("parses");
        assert!(empty_read.server_config().is_err());
        let empty_write =
            args(&["--auth-mode", "public-pull", "--write-apikey", " "]).expect("parses");
        assert!(empty_write.server_config().is_err());
    }

    #[test]
    fn the_upload_ceiling_defaults_high_and_can_be_removed() {
        // The failure this guards against is silent in the worst way: a limit
        // below a real layer turns every push of a big image into a `413` that
        // no retry can fix, and the client's message names a byte count rather
        // than the flag that produced it.
        let ceiling = |extra: &[&str]| {
            let (config, _) = args(extra).expect("parses").server_config().expect("valid");
            config.max_upload_bytes
        };

        assert_eq!(ceiling(&[]), Some(DEFAULT_MAX_UPLOAD_BYTES));
        const {
            assert!(
                DEFAULT_MAX_UPLOAD_BYTES > 8 * 1024 * 1024 * 1024,
                "the default must clear the largest layers people actually push",
            )
        };
        assert_eq!(
            ceiling(&["--max-upload-bytes", "0"]),
            None,
            "zero removes the ceiling rather than rejecting every body",
        );
        assert_eq!(ceiling(&["--max-upload-bytes", "4096"]), Some(4096));
    }

    #[test]
    fn the_modes_are_spelled_as_one_ladder() {
        // The flag answers one question - how open is this registry - and the
        // three values are its only answers, from open to closed.
        for (spelling, mode) in [
            ("open", AuthMode::Open),
            ("public-pull", AuthMode::PublicPull),
            ("private", AuthMode::Private),
        ] {
            let args = args(&["--auth-mode", spelling]).expect("parses");
            assert_eq!(args.auth_mode, mode);
        }
        // Every older spelling has to fail loudly rather than mean something
        // new: `--auth all` once required a key for everything, and silently
        // reading it as anything else would serve a more open registry than
        // the command line asks for. clap does not infer long flags, so the
        // retired flag is an error on its own.
        assert!(args(&["--auth", "all"]).is_err(), "the retired flag");
        for retired in ["none", "write", "all"] {
            assert!(
                args(&["--auth-mode", retired]).is_err(),
                "the retired value `{retired}`"
            );
        }
    }
}
