## StandX CLI v1.2.0

Self-update.

```bash
standx update --check     # compare installed vs latest release, change nothing
standx update             # download, verify sha256, replace this binary
standx --yes update       # no prompt (also STANDX_AUTO_CONFIRM=true)
standx update --pre       # allow pre-release candidates
```

### What it does

Downloads the release asset for this platform over TLS, verifies its SHA-256
against the `checksums.txt` published beside it, unpacks it, asks the new binary
for its own `--version` to confirm it matches the release, and only then
atomically renames it over the running executable. **Any failure leaves the
existing binary untouched.**

Stable checks resolve the latest tag through the `releases/latest` redirect
rather than the REST API, so they are not subject to GitHub's 60-per-hour
unauthenticated API limit — a shared or NAT'd egress IP would otherwise meet a
bare `HTTP 403` on a perfectly healthy install. Only `--pre` needs the API, and
it names the rate limit explicitly (and honours `GITHUB_TOKEN`).

### What it refuses to do

- **It leaves Homebrew alone.** A binary under a Cellar path is refused with a
  pointer to `brew upgrade standx-cli`, so the formula and the installed binary
  cannot silently diverge.
- **It never elevates privileges.** An unwritable install directory is an error
  with instructions, not a `sudo` invocation.
- **It never guesses a version.** An unparseable version string aborts, because
  that comparison is the only gate on overwriting a binary.

### Please read before trusting it

Checksum verification protects against a **corrupted or truncated download, not
against a compromised release** — the checksum ships from the same place as the
archive. Real provenance needs a detached signature verified against a key
shipped in the binary, and that is **not implemented** (tracked in #336).

Because of that, the one place this code executes the downloaded binary (the
`--version` probe) runs with a cleared environment and a minimal allow-list, so a
hostile release cannot read this process's `STANDX_JWT`, `STANDX_PRIVATE_KEY` or
`GITHUB_TOKEN` on its way in. That bounds the damage; it does not remove the need
for signing.

Two behavioural gaps are known and tracked rather than hidden (#337):
`--force` currently permits a silent downgrade, and `--pre` can select an
unpublished draft release when run with repository push credentials in the
environment.

### Fixed

- The update command's `--yes` is the existing global flag rather than a second
  copy of it. A duplicate long option makes clap panic in debug builds
  (`Long option names must be unique`) while release builds compile that
  assertion out — which is exactly how it survived manual smoke testing. A test
  now runs clap's `debug_assert()` over the update subtree so this class of bug
  cannot ship again.

Unrelated and still open: `standx block list --help` panics in debug builds
because `-s` is claimed by both `--symbol` and `--status`. Fixing that changes a
published short flag, so it is its own change — which is why the new assertion is
scoped to the update subtree rather than the whole command tree.

### Upgrading to this release

`standx update` only exists **from** this version, so this one time you still
need the installer or Homebrew:

```bash
curl -sSL https://raw.githubusercontent.com/wjllance/standx-cli/main/install.sh | sh
# or
brew upgrade standx-cli
```

Note that the Homebrew formula may lag: the tap update job is currently failing
on credentials (#338), so `brew` can still be serving an older version.
