#!/bin/sh
# StandX CLI One-line Installer
# Supports macOS (Apple Silicon) and Linux (x86_64/ARM64)

set -e

REPO="wjllance/standx-cli"
BINARY_NAME="standx"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

say() {
    printf '%b\n' "$*"
}

warn() {
    printf '%b\n' "${YELLOW}$*${NC}" >&2
}

die() {
    printf '%b\n' "${RED}Error: $*${NC}" >&2
    exit 1
}

# Detect target platform
get_target() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64)
                    echo "aarch64-apple-darwin"
                    ;;
                *)
                    die "Unsupported macOS architecture: $arch"
                    ;;
            esac
            ;;
        Linux)
            case "$arch" in
                aarch64|arm64)
                    echo "aarch64-unknown-linux-gnu"
                    ;;
                x86_64|amd64)
                    echo "x86_64-unknown-linux-gnu"
                    ;;
                *)
                    die "Unsupported Linux architecture: $arch"
                    ;;
            esac
            ;;
        *)
            die "Unsupported operating system: $os"
            ;;
    esac
}

# Get latest version tag.
# Prefers the github.com redirect (not subject to API rate limits) and falls
# back to the REST API. Prints nothing and returns 1 when both fail.
get_latest_tag() {
    tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest" 2>/dev/null \
        | sed -n 's#.*/releases/tag/##p')

    if [ -z "$tag" ]; then
        tag=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
            | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    fi

    [ -n "$tag" ] || return 1
    echo "$tag"
}

# Compute the SHA256 of a file. Returns 2 when no hashing tool is available.
sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        return 2
    fi
}

binary_version() {
    binary_path=$1
    output=$(env -i \
        PATH="${PATH:-/usr/bin:/bin}" \
        HOME="${HOME:-/}" \
        LANG="${LANG:-C}" \
        TMPDIR="${TMPDIR:-/tmp}" \
        "$binary_path" --version 2>/dev/null) || return 1
    printf '%s\n' "$output" | awk '{print $NF}'
}

# Main installation logic
main() {
    say "${GREEN}=== StandX CLI Installer ===${NC}"
    echo ""

    command -v curl >/dev/null 2>&1 || die "curl is required but was not found"
    command -v tar >/dev/null 2>&1 || die "tar is required but was not found"
    command -v env >/dev/null 2>&1 || die "env is required but was not found"

    # A standalone install must remain writable by the user so `standx update`
    # can atomically replace it later without elevating privileges.
    if [ -z "${INSTALL_DIR:-}" ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set; choose an install directory with INSTALL_DIR"
        INSTALL_DIR="${HOME}/.local/bin"
    fi
    if [ ! -d "$INSTALL_DIR" ]; then
        if ! mkdir -p "$INSTALL_DIR"; then
            die "Unable to create install directory $INSTALL_DIR.
Choose a directory you own, for example:
  INSTALL_DIR=\"\$HOME/.local/bin\""
        fi
    fi
    if [ ! -w "$INSTALL_DIR" ]; then
        die "Install directory $INSTALL_DIR is not writable.
Choose a directory you own, for example:
  INSTALL_DIR=\"\$HOME/.local/bin\"
This installer deliberately does not elevate privileges. For a system-managed
macOS install, use Homebrew instead."
    fi

    staging_dir=
    tmp_dir=
    cleanup() {
        [ -z "${tmp_dir:-}" ] || rm -rf "$tmp_dir"
        [ -z "${staging_dir:-}" ] || rm -rf "$staging_dir"
    }
    trap cleanup 0 HUP INT TERM
    staging_dir=$(mktemp -d "${INSTALL_DIR}/.standx-install.XXXXXX") \
        || die "Unable to create a staging directory in $INSTALL_DIR"
    tmp_dir=$(mktemp -d) || die "Unable to create a temporary download directory"

    # Detect platform
    target=$(get_target)
    say "Detected platform: ${YELLOW}$target${NC}"

    # Resolve version (STANDX_VERSION overrides discovery, e.g. STANDX_VERSION=vX.Y.Z)
    if [ -n "${STANDX_VERSION:-}" ]; then
        tag="$STANDX_VERSION"
        say "Requested version: ${YELLOW}$tag${NC}"
    else
        echo "Fetching latest version information..."
        tag=$(get_latest_tag) || die "Unable to determine the latest version.
GitHub may be rate-limiting or blocking this network. Retry later, or pin a
version explicitly:
  curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | STANDX_VERSION=vX.Y.Z sh"
        say "Latest version: ${YELLOW}$tag${NC}"
    fi

    # Construct download URL
    tarball_name="${BINARY_NAME}-${tag}-${target}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${tag}/${tarball_name}"
    checksums_url="https://github.com/${REPO}/releases/download/${tag}/checksums.txt"

    # Download tarball
    echo ""
    echo "Downloading ${tarball_name}..."
    if ! curl -fsSL -o "${tmp_dir}/${tarball_name}" "$download_url"; then
        die "Download failed: $download_url
No release asset for this platform and version. Check
https://github.com/${REPO}/releases/tag/${tag} for available downloads."
    fi

    # Download checksums.txt
    echo "Downloading checksums.txt..."
    if ! curl -fsSL -o "${tmp_dir}/checksums.txt" "$checksums_url"; then
        die "Unable to download checksums.txt; nothing was installed"
    fi
    echo "Verifying file integrity..."
    expected=$(awk -v f="$tarball_name" \
        '$2 == f || $2 == "*" f {print $1; exit}' "${tmp_dir}/checksums.txt")
    if [ -z "$expected" ]; then
        die "${tarball_name} is not listed in checksums.txt; nothing was installed"
    fi
    case "$expected" in
        *[!0-9a-fA-F]*)
            die "Checksum entry for ${tarball_name} is not a SHA-256 digest; nothing was installed"
            ;;
    esac
    if [ "${#expected}" -ne 64 ]; then
        die "Checksum entry for ${tarball_name} is not a SHA-256 digest; nothing was installed"
    fi
    if ! actual=$(sha256_of "${tmp_dir}/${tarball_name}"); then
        die "No SHA-256 tool (shasum or sha256sum) was found; nothing was installed"
    fi
    if [ "$actual" != "$expected" ]; then
        die "SHA256 verification failed, file may be corrupted or tampered
  expected: $expected
  actual:   $actual
Nothing was installed."
    fi
    say "${GREEN}✓ Verification passed${NC}"

    # Extract into the destination filesystem so the final rename is atomic.
    echo ""
    echo "Extracting..."
    if ! tar -xzf "${tmp_dir}/${tarball_name}" -C "$staging_dir" standx; then
        die "Unable to extract standx from ${tarball_name}; nothing was installed"
    fi

    # Check extracted binary
    binary_path="${staging_dir}/${BINARY_NAME}"
    if [ ! -f "$binary_path" ] || [ -L "$binary_path" ]; then
        die "Release archive did not contain a regular ${BINARY_NAME} binary; nothing was installed"
    fi
    chmod +x "$binary_path" || die "Unable to mark the new binary executable; nothing was installed"

    # Run the downloaded binary with a cleared environment before it replaces
    # anything. Checksums protect integrity, not release provenance, so secrets
    # such as STANDX_JWT and STANDX_PRIVATE_KEY must not be inherited here.
    expected_version=${tag#v}
    if ! reported_version=$(binary_version "$binary_path"); then
        die "Downloaded binary could not run on this machine; nothing was installed"
    fi
    if [ "$reported_version" != "$expected_version" ]; then
        die "Downloaded binary reports ${reported_version}, but the release is ${expected_version}; nothing was installed"
    fi

    # The staged binary and destination share a filesystem, so this replacement
    # is atomic even when an older standx already exists.
    install_path="${INSTALL_DIR}/${BINARY_NAME}"
    echo ""
    echo "Installing to ${install_path}..."
    mv -f "$binary_path" "$install_path" \
        || die "Unable to atomically install to ${install_path}; the previous binary was left unchanged"

    # Verify the exact installed path, not whichever older copy PATH resolves.
    echo ""
    echo "Verifying installation..."
    installed_version=$(binary_version "$install_path") \
        || die "Installed binary at ${install_path} could not report its version"
    if [ "$installed_version" != "$expected_version" ]; then
        die "Installed binary at ${install_path} reports ${installed_version}, expected ${expected_version}"
    fi
    say "${GREEN}✓ Installation successful!${NC}"
    echo ""
    say "Version: ${YELLOW}${BINARY_NAME} ${installed_version}${NC}"

    resolved=$(command -v "$BINARY_NAME" 2>/dev/null || true)
    if [ "$resolved" != "$install_path" ]; then
        if [ -n "$resolved" ]; then
            warn "Warning: PATH currently resolves ${BINARY_NAME} to ${resolved}, not ${install_path}"
        else
            warn "Warning: ${install_path} is not currently in PATH"
        fi
        echo "Add the install directory before other copies:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi

    echo ""
    echo "Get started with:"
    say "  ${YELLOW}standx --help${NC}          Show help"
    say "  ${YELLOW}standx --version${NC}       Show version"
    say "  ${YELLOW}standx auth login${NC}      Authenticate"
}

# Run main function
main "$@"
