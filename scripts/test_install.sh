#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "${SCRIPT_DIR}/.." && pwd)
INSTALLER_UNDER_TEST=${INSTALLER_UNDER_TEST:-"${REPO_ROOT}/install.sh"}
TEST_ROOT=$(mktemp -d)
trap 'rm -rf "$TEST_ROOT"' 0 HUP INT TERM

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

clean_version() {
    env -i PATH="/usr/bin:/bin" "$1" --version
}

case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64)
        TARGET=aarch64-apple-darwin
        ;;
    Linux:x86_64|Linux:amd64)
        TARGET=x86_64-unknown-linux-gnu
        ;;
    Linux:aarch64|Linux:arm64)
        TARGET=aarch64-unknown-linux-gnu
        ;;
    *)
        fail "unsupported test platform"
        ;;
esac

TAG=v9.8.7
ASSET="standx-${TAG}-${TARGET}.tar.gz"
FAKE_BIN="${TEST_ROOT}/fake-bin"
RELEASE_DIR="${TEST_ROOT}/release"
PAYLOAD_DIR="${TEST_ROOT}/payload"
mkdir -p "$FAKE_BIN" "$RELEASE_DIR" "$PAYLOAD_DIR"

cat >"${FAKE_BIN}/curl" <<'EOF'
#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            output=$2
            shift 2
            ;;
        -*)
            shift
            ;;
        *)
            url=$1
            shift
            ;;
    esac
done

[ -n "$output" ] && [ -n "$url" ] || exit 2
asset=${url##*/}
if [ "$asset" = "checksums.txt" ] && [ "${FAKE_CHECKSUM_FAILURE:-0}" = "1" ]; then
    exit 22
fi
cp "${FAKE_RELEASE_DIR}/${asset}" "$output"
EOF

cat >"${FAKE_BIN}/sudo" <<'EOF'
#!/bin/sh
printf 'called\n' >"${FAKE_TEST_ROOT}/sudo-called"
exit 99
EOF

cat >"${FAKE_BIN}/standx" <<'EOF'
#!/bin/sh
printf 'standx 0.1.0\n'
EOF

chmod +x "${FAKE_BIN}/curl" "${FAKE_BIN}/sudo" "${FAKE_BIN}/standx"

build_release() {
    version=$1
    {
        printf '%s\n' '#!/bin/sh'
        printf '%s\n' 'if [ -n "${STANDX_JWT:-}" ] || [ -n "${STANDX_PRIVATE_KEY:-}" ]; then'
        printf '%s\n' '    echo "secret environment leaked into version probe" >&2'
        printf '%s\n' '    exit 42'
        printf '%s\n' 'fi'
        printf 'printf "standx %s\\n"\n' "$version"
    } >"${PAYLOAD_DIR}/standx"
    chmod +x "${PAYLOAD_DIR}/standx"
    tar -czf "${RELEASE_DIR}/${ASSET}" -C "$PAYLOAD_DIR" standx
    digest=$(sha256_of "${RELEASE_DIR}/${ASSET}")
    printf '%s  %s\n' "$digest" "$ASSET" >"${RELEASE_DIR}/checksums.txt"
}

assert_no_staging_dirs() {
    install_dir=$1
    leftovers=$(find "$install_dir" -maxdepth 1 -name '.standx-install.*' -print)
    [ -z "$leftovers" ] || fail "installer left staging directories behind: $leftovers"
}

build_release 9.8.7
success_home="${TEST_ROOT}/success-home"
success_output="${TEST_ROOT}/success-output"
HOME="$success_home" \
PATH="${FAKE_BIN}:$PATH" \
STANDX_VERSION="$TAG" \
STANDX_JWT=secret-jwt \
STANDX_PRIVATE_KEY=secret-key \
FAKE_RELEASE_DIR="$RELEASE_DIR" \
FAKE_TEST_ROOT="$TEST_ROOT" \
sh "$INSTALLER_UNDER_TEST" >"$success_output" 2>&1

installed="${success_home}/.local/bin/standx"
[ -x "$installed" ] || fail "default install did not create $installed"
[ "$(clean_version "$installed")" = "standx 9.8.7" ] || fail "installed version is wrong"
[ ! -e "${TEST_ROOT}/sudo-called" ] || fail "installer invoked sudo"
grep -q "PATH currently resolves standx to ${FAKE_BIN}/standx" "$success_output" \
    || fail "installer did not report the shadowing PATH entry"
assert_no_staging_dirs "${success_home}/.local/bin"

failure_home="${TEST_ROOT}/checksum-failure-home"
mkdir -p "${failure_home}/.local/bin"
cp "${FAKE_BIN}/standx" "${failure_home}/.local/bin/standx"
if HOME="$failure_home" \
    PATH="${FAKE_BIN}:$PATH" \
    STANDX_VERSION="$TAG" \
    FAKE_CHECKSUM_FAILURE=1 \
    FAKE_RELEASE_DIR="$RELEASE_DIR" \
    FAKE_TEST_ROOT="$TEST_ROOT" \
    sh "$INSTALLER_UNDER_TEST" >"${TEST_ROOT}/checksum-failure-output" 2>&1
then
    fail "installer succeeded without checksums.txt"
fi
[ "$(clean_version "${failure_home}/.local/bin/standx")" = "standx 0.1.0" ] \
    || fail "checksum failure replaced the existing binary"
assert_no_staging_dirs "${failure_home}/.local/bin"

build_release 9.8.6
mismatch_home="${TEST_ROOT}/version-mismatch-home"
mkdir -p "${mismatch_home}/.local/bin"
cp "${FAKE_BIN}/standx" "${mismatch_home}/.local/bin/standx"
if HOME="$mismatch_home" \
    PATH="${FAKE_BIN}:$PATH" \
    STANDX_VERSION="$TAG" \
    FAKE_RELEASE_DIR="$RELEASE_DIR" \
    FAKE_TEST_ROOT="$TEST_ROOT" \
    sh "$INSTALLER_UNDER_TEST" >"${TEST_ROOT}/version-mismatch-output" 2>&1
then
    fail "installer accepted a binary whose version did not match the release"
fi
[ "$(clean_version "${mismatch_home}/.local/bin/standx")" = "standx 0.1.0" ] \
    || fail "version mismatch replaced the existing binary"
assert_no_staging_dirs "${mismatch_home}/.local/bin"

printf 'install.sh tests passed\n'
