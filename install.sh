#!/bin/sh
# StandX CLI One-line Installer
# Supports macOS (Apple Silicon) and Linux (x86_64/ARM64)

set -e

REPO="wjllance/standx-cli"
BINARY_NAME="standx"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

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

# Main installation logic
main() {
    say "${GREEN}=== StandX CLI Installer ===${NC}"
    echo ""

    command -v curl >/dev/null 2>&1 || die "curl is required but was not found"
    command -v tar >/dev/null 2>&1 || die "tar is required but was not found"

    # Detect platform
    target=$(get_target)
    say "Detected platform: ${YELLOW}$target${NC}"

    # Resolve version (STANDX_VERSION overrides discovery, e.g. STANDX_VERSION=v1.2.0)
    if [ -n "${STANDX_VERSION:-}" ]; then
        tag="$STANDX_VERSION"
        say "Requested version: ${YELLOW}$tag${NC}"
    else
        echo "Fetching latest version information..."
        tag=$(get_latest_tag) || die "Unable to determine the latest version.
GitHub may be rate-limiting or blocking this network. Retry later, or pin a
version explicitly:
  curl -sSL https://raw.githubusercontent.com/${REPO}/main/install.sh | STANDX_VERSION=v1.2.0 sh"
        say "Latest version: ${YELLOW}$tag${NC}"
    fi

    # Construct download URL
    tarball_name="${BINARY_NAME}-${tag}-${target}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${tag}/${tarball_name}"
    checksums_url="https://github.com/${REPO}/releases/download/${tag}/checksums.txt"

    # Create temp directory
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

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
        warn "Warning: Unable to download checksums.txt, skipping verification"
    else
        echo "Verifying file integrity..."
        expected=$(awk -v f="$tarball_name" \
            '$2 == f || $2 == "*" f {print $1; exit}' "${tmp_dir}/checksums.txt")
        if [ -z "$expected" ]; then
            warn "Warning: ${tarball_name} is not listed in checksums.txt, skipping verification"
        elif ! actual=$(sha256_of "${tmp_dir}/${tarball_name}"); then
            warn "Warning: no sha256 tool (shasum/sha256sum) found, skipping verification"
        elif [ "$actual" != "$expected" ]; then
            die "SHA256 verification failed, file may be corrupted or tampered
  expected: $expected
  actual:   $actual"
        else
            say "${GREEN}✓ Verification passed${NC}"
        fi
    fi

    # Extract
    echo ""
    echo "Extracting..."
    tar -xzf "${tmp_dir}/${tarball_name}" -C "$tmp_dir"

    # Check extracted binary
    binary_path="${tmp_dir}/${BINARY_NAME}"
    if [ ! -f "$binary_path" ]; then
        die "Binary file ${BINARY_NAME} not found after extraction"
    fi

    # Check install directory permissions
    if [ ! -d "$INSTALL_DIR" ]; then
        warn "Install directory $INSTALL_DIR does not exist, attempting to create..."
        sudo mkdir -p "$INSTALL_DIR" || die "Unable to create install directory"
    fi

    # Install
    echo ""
    echo "Installing to ${INSTALL_DIR}/${BINARY_NAME}..."
    if [ -w "$INSTALL_DIR" ]; then
        mv "$binary_path" "${INSTALL_DIR}/${BINARY_NAME}"
        chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
    else
        warn "Administrator privileges required to install to $INSTALL_DIR"
        sudo mv "$binary_path" "${INSTALL_DIR}/${BINARY_NAME}" \
            || die "Unable to install to ${INSTALL_DIR} (sudo failed)"
        sudo chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
    fi

    # Verify installation
    echo ""
    echo "Verifying installation..."
    if command -v "$BINARY_NAME" >/dev/null 2>&1; then
        version=$($BINARY_NAME --version 2>/dev/null || echo "unknown")
        say "${GREEN}✓ Installation successful!${NC}"
        echo ""
        say "Version: ${YELLOW}$version${NC}"
        echo ""
        echo "Get started with:"
        say "  ${YELLOW}standx --help${NC}          Show help"
        say "  ${YELLOW}standx --version${NC}       Show version"
        say "  ${YELLOW}standx auth login${NC}      Authenticate"
    else
        warn "Warning: Installation complete, but $BINARY_NAME is not in PATH"
        echo "Please ensure $INSTALL_DIR is in your PATH environment variable"
    fi
}

# Run main function
main "$@"
