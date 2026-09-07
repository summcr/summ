#!/bin/sh
#
# install.sh — download the summ binary for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/summcr/summ/main/scripts/install.sh | sh
#   ./scripts/install.sh --dir /usr/local/bin --version v0.1.0-rc.1
#
# It resolves uname to one of the four published targets, downloads that
# tarball and its .sha256 companion, verifies the checksum, and extracts a
# single `summ` into the install directory. Nothing else: no PATH edits, no
# service files, no sudo — an installer that rewrites a shell profile is one
# nobody can predict from reading a one-liner.
#
# POSIX sh rather than bash, because the one-liner above is piped to whatever
# `sh` is on the machine and that is dash on Debian and Ubuntu.
#
#   SUMM_INSTALL_DIR   where to put the binary   (default: .)
#   SUMM_VERSION       release tag to install    (default: the latest release)
#
# Requires: curl or wget, tar, and sha256sum or shasum.

set -eu

REPO="summcr/summ"
INSTALL_DIR="${SUMM_INSTALL_DIR:-.}"
VERSION="${SUMM_VERSION:-}"

while [ $# -gt 0 ]; do
	case "$1" in
	-d | --dir)
		INSTALL_DIR="${2:?--dir needs a directory}"
		shift 2
		;;
	-v | --version)
		VERSION="${2:?--version needs a release tag}"
		shift 2
		;;
	-h | --help)
		sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		echo "install.sh: unknown argument: $1" >&2
		exit 2
		;;
	esac
done

die() {
	echo "install.sh: $*" >&2
	exit 1
}

# --------------------------------------------------------------- platform ---

# The four targets `.github/workflows/release.yml` builds. Anything else is an
# error naming what was found, not a guess at the nearest binary: a wrong-arch
# download fails later, further from the cause, and on Linux it fails as
# "cannot execute binary file" with no hint of why.
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
Linux) os_part="unknown-linux-gnu" ;;
Darwin) os_part="apple-darwin" ;;
*) die "no prebuilt binary for $os. Build from source: cargo install --path summ-server" ;;
esac

case "$arch" in
x86_64 | amd64) arch_part="x86_64" ;;
aarch64 | arm64) arch_part="aarch64" ;;
*) die "no prebuilt binary for $arch on $os. Build from source: cargo install --path summ-server" ;;
esac

target="${arch_part}-${os_part}"
asset="summ-${target}.tar.gz"

# A stable asset name is what makes `releases/latest/download/<asset>` a URL
# that never changes; a pinned version addresses the same name under its tag.
if [ -n "$VERSION" ]; then
	base="https://github.com/${REPO}/releases/download/${VERSION}"
else
	base="https://github.com/${REPO}/releases/latest/download"
fi

# --------------------------------------------------------------- download ---

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -qO "$2" "$1"; }
else
	die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	die "need sha256sum or shasum"
fi

command -v tar >/dev/null 2>&1 || die "need tar"

[ -d "$INSTALL_DIR" ] || die "not a directory: $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || die "not writable: $INSTALL_DIR (try sudo, or --dir)"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "install.sh: downloading $asset (${VERSION:-latest})"
fetch "${base}/${asset}" "${tmp}/${asset}" ||
	die "download failed: ${base}/${asset}"

# The release publishes a .sha256 beside every tarball, so verifying costs one
# more small request. A truncated download is the common case this catches —
# tar would report it too, but as a corrupt archive rather than as a bad byte
# count, and the two want different responses.
if fetch "${base}/${asset}.sha256" "${tmp}/${asset}.sha256" 2>/dev/null; then
	want="$(cut -d' ' -f1 <"${tmp}/${asset}.sha256")"
	got="$(sha256 "${tmp}/${asset}")"
	[ "$want" = "$got" ] || die "checksum mismatch for $asset: expected $want, got $got"
	echo "install.sh: checksum ok"
else
	echo "install.sh: no published checksum for $asset, skipping verification" >&2
fi

# Extract into the temp directory and move the binary, rather than untarring
# over the install directory: the tarball also carries LICENSE and NOTICE, and
# an installer that drops those into whatever directory it was run from is
# doing something the one-liner did not say it would.
tar -xzf "${tmp}/${asset}" -C "$tmp" summ || die "could not extract summ from $asset"
chmod +x "${tmp}/summ"
mv "${tmp}/summ" "${INSTALL_DIR}/summ"

installed="${INSTALL_DIR}/summ"

# Run it once before claiming success. A binary that downloaded and unpacked
# perfectly still may not run here — the Linux builds have a glibc floor — and
# that is worth hearing from the installer rather than from `summ serve`.
if version="$("$installed" --version 2>&1)"; then
	echo "install.sh: installed $version to $installed"
	echo
	echo "  $installed serve"
else
	echo "install.sh: installed $installed, but it does not run here:" >&2
	echo "  $version" >&2
	exit 1
fi
