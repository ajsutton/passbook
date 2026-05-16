#!/usr/bin/env bash
# bump-reth.sh — one-command lockstep bump of the reth / op-reth source pins.
#
# This SUPERSEDES the plan's original single-`rev` sed draft. There are TWO
# pinned revs that MUST move together (see docs/reth-pin.md "Why co-resolution
# works"):
#
#   * ethereum-optimism/optimism monorepo rev  → used by `reth-op` and
#     `reth-optimism-evm` in the root Cargo.toml, AND duplicated as the
#     default `OPTIMISM_REV` in scripts/seed-vendor.sh.
#   * paradigmxyz/reth rev → used by `reth-ethereum` and
#     `reth-exex-test-utils` in the root Cargo.toml. This MUST equal the
#     `paradigmxyz/reth` rev the chosen optimism monorepo pins all of its
#     upstream reth crates to (read the monorepo's `rust/Cargo.toml`). If the
#     two drift, co-resolution breaks (duplicate, incompatible reth types).
#
# What this script does (it AUTOMATES exactly the "Bump procedure" in
# docs/reth-pin.md):
#   1. Validate args; detect a no-op (already at these revs) and exit clean.
#   2. Update BOTH revs in every committed file: root Cargo.toml (the four
#      dependency lines) and the `OPTIMISM_REV=` default in seed-vendor.sh.
#      Uses precise per-line sed — it does NOT blindly replace every 40-hex
#      string. Asserts (via grep) that the old revs are gone and the new ones
#      present; fails loudly otherwise.
#   3. Re-seed the local mirror for the NEW optimism rev (rm + seed-vendor.sh).
#   4. Regenerate / retarget Cargo.lock for the new revs following the
#      documented mechanism, validating with the spike co-resolution gate.
#   5. Re-run `cargo build -p spike --locked` (the co-resolution gate). On
#      failure, print the error and exit non-zero WITHOUT committing — the
#      working tree is left for the operator to review.
#
# This script NEVER git-commits. The operator reviews the working tree, runs
# `make verify-pin`, then commits.
#
# Idempotent: re-running with the revs currently pinned is a clean no-op.
#
# Usage:
#   scripts/bump-reth.sh --optimism-rev <SHA> --reth-rev <SHA>
#   scripts/bump-reth.sh --optimism-rev <SHA>   # prints how to find the
#                                               # matching --reth-rev, then errors
set -euo pipefail

# --- Derive repo root from this script's own location (no hardcoded path) ----
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd -P)"

CARGO_TOML="${REPO_ROOT}/Cargo.toml"
CARGO_LOCK="${REPO_ROOT}/Cargo.lock"
SEED_SCRIPT="${REPO_ROOT}/scripts/seed-vendor.sh"
CARGO_CONFIG="${REPO_ROOT}/.cargo/config.toml"

OPTIMISM_REMOTE="https://github.com/ethereum-optimism/optimism"
RETH_REMOTE="https://github.com/paradigmxyz/reth"

die()  { echo "bump-reth: ERROR: $*" >&2; exit 1; }
info() { echo "bump-reth: $*"; }

usage() {
  cat >&2 <<EOF
Usage: scripts/bump-reth.sh --optimism-rev <SHA> --reth-rev <SHA>

Both revs are REQUIRED and move in LOCKSTEP:

  --optimism-rev <SHA>  new ethereum-optimism/optimism monorepo 'develop' SHA
                        (drives reth-op / reth-optimism-evm in Cargo.toml and
                        OPTIMISM_REV in scripts/seed-vendor.sh)
  --reth-rev <SHA>      new paradigmxyz/reth SHA. This MUST be the rev that the
                        chosen optimism monorepo pins its upstream reth crates
                        to. Find it by reading, in the monorepo at the new
                        optimism rev:

                            rust/Cargo.toml   (the paradigmxyz/reth 'rev = ...'
                                               that ALL upstream reth crates use)

                        e.g.:
                          git clone --depth 1 ${OPTIMISM_REMOTE} /tmp/op \\
                            && git -C /tmp/op fetch --depth 1 origin <opt-sha> \\
                            && git -C /tmp/op checkout FETCH_HEAD \\
                            && grep 'paradigmxyz/reth' /tmp/op/rust/Cargo.toml

This script does NOT guess --reth-rev: the two revs must be explicitly
unified by the operator. See docs/reth-pin.md "Bump procedure".
EOF
  exit 2
}

is_sha() { [[ "$1" =~ ^[0-9a-f]{40}$ ]]; }

# --- Parse args --------------------------------------------------------------
OPT_REV=""
RETH_REV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --optimism-rev) OPT_REV="${2:-}"; shift 2 ;;
    --reth-rev)     RETH_REV="${2:-}"; shift 2 ;;
    -h|--help)      usage ;;
    *)              echo "bump-reth: unknown argument: $1" >&2; usage ;;
  esac
done

if [ -z "${OPT_REV}" ]; then
  echo "bump-reth: --optimism-rev is required" >&2
  usage
fi

if [ -z "${RETH_REV}" ]; then
  cat >&2 <<EOF
bump-reth: --optimism-rev given without --reth-rev.

The two revs MUST move together. To find the matching paradigmxyz/reth rev
for optimism monorepo rev ${OPT_REV}, read that monorepo's rust/Cargo.toml:

  git clone --depth 1 ${OPTIMISM_REMOTE} /tmp/op-bump \\
    && git -C /tmp/op-bump fetch --depth 1 origin ${OPT_REV} \\
    && git -C /tmp/op-bump checkout FETCH_HEAD \\
    && grep -m1 'paradigmxyz/reth' /tmp/op-bump/rust/Cargo.toml

Take the 'rev = "<40-hex>"' it pins all upstream reth crates to and re-run:

  scripts/bump-reth.sh --optimism-rev ${OPT_REV} --reth-rev <that-sha>

--reth-rev is required to proceed (the script will not guess it).
EOF
  exit 2
fi

is_sha "${OPT_REV}"  || die "--optimism-rev must be a 40-char lowercase hex SHA (got: ${OPT_REV})"
is_sha "${RETH_REV}" || die "--reth-rev must be a 40-char lowercase hex SHA (got: ${RETH_REV})"

[ -f "${CARGO_TOML}" ]  || die "missing ${CARGO_TOML}"
[ -f "${SEED_SCRIPT}" ] || die "missing ${SEED_SCRIPT}"

# --- Discover the currently pinned revs --------------------------------------
cur_opt_rev="$(grep -E '^reth-op = \{' "${CARGO_TOML}" \
  | grep -oE 'rev = "[0-9a-f]{40}"' | head -1 | grep -oE '[0-9a-f]{40}' || true)"
cur_reth_rev="$(grep -E '^reth-ethereum = \{' "${CARGO_TOML}" \
  | grep -oE 'rev = "[0-9a-f]{40}"' | head -1 | grep -oE '[0-9a-f]{40}' || true)"
cur_seed_rev="$(grep -E '^OPTIMISM_REV=' "${SEED_SCRIPT}" \
  | grep -oE '[0-9a-f]{40}' | head -1 || true)"

[ -n "${cur_opt_rev}" ]  || die "could not find current optimism rev (reth-op line) in Cargo.toml"
[ -n "${cur_reth_rev}" ] || die "could not find current reth rev (reth-ethereum line) in Cargo.toml"
[ -n "${cur_seed_rev}" ] || die "could not find current OPTIMISM_REV in scripts/seed-vendor.sh"

info "current optimism rev (Cargo.toml reth-op)       = ${cur_opt_rev}"
info "current optimism rev (seed-vendor OPTIMISM_REV)  = ${cur_seed_rev}"
info "current reth     rev (Cargo.toml reth-ethereum)  = ${cur_reth_rev}"
info "requested optimism rev = ${OPT_REV}"
info "requested reth     rev = ${RETH_REV}"

if [ "${cur_opt_rev}" != "${cur_seed_rev}" ]; then
  die "PRE-EXISTING DRIFT: Cargo.toml optimism rev (${cur_opt_rev}) != seed-vendor OPTIMISM_REV (${cur_seed_rev}). Fix manually before bumping."
fi

# --- Idempotence: already at the requested revs => clean no-op ---------------
if [ "${cur_opt_rev}" = "${OPT_REV}" ] && [ "${cur_reth_rev}" = "${RETH_REV}" ]; then
  info "already at these revs (optimism=${OPT_REV}, reth=${RETH_REV}) — nothing to do."
  info "no files modified. (run 'make verify-pin' if you want to re-validate the build.)"
  exit 0
fi

info "bumping: optimism ${cur_opt_rev} -> ${OPT_REV}"
info "bumping: reth     ${cur_reth_rev} -> ${RETH_REV}"

# --- 1. Precise rev rewrites in committed files ------------------------------
# sed -i portably (BSD/macOS + GNU): use a temp backup suffix then delete it.
sed_inplace() {
  local file="$1"; shift
  sed -i.bumpbak "$@" "${file}"
  rm -f "${file}.bumpbak"
}

# Cargo.toml: only the four specific dependency lines, keyed by crate name at
# line start, replacing just the rev string on that line.
sed_inplace "${CARGO_TOML}" \
  -e "/^reth-op = {/s/rev = \"${cur_opt_rev}\"/rev = \"${OPT_REV}\"/" \
  -e "/^reth-optimism-evm = {/s/rev = \"${cur_opt_rev}\"/rev = \"${OPT_REV}\"/" \
  -e "/^reth-ethereum = {/s/rev = \"${cur_reth_rev}\"/rev = \"${RETH_REV}\"/" \
  -e "/^reth-exex-test-utils = {/s/rev = \"${cur_reth_rev}\"/rev = \"${RETH_REV}\"/"

# seed-vendor.sh: only the OPTIMISM_REV= default assignment.
sed_inplace "${SEED_SCRIPT}" \
  -e "s/OPTIMISM_REV:-${cur_opt_rev}/OPTIMISM_REV:-${OPT_REV}/"

# --- 2. Assert old revs gone, new revs present (fail loudly) -----------------
assert_absent() {
  local label="$1" pattern="$2" file="$3"
  if grep -qE "${pattern}" "${file}"; then
    die "${label}: stale rev still present in ${file} after rewrite"
  fi
}
assert_present() {
  local label="$1" pattern="$2" file="$3"
  if ! grep -qE "${pattern}" "${file}"; then
    die "${label}: expected new rev NOT found in ${file} after rewrite"
  fi
}

assert_absent  "Cargo.toml reth-op old optimism rev"  "^reth-op = \{.*rev = \"${cur_opt_rev}\"" "${CARGO_TOML}"
assert_present "Cargo.toml reth-op new optimism rev"  "^reth-op = \{.*rev = \"${OPT_REV}\""     "${CARGO_TOML}"
assert_absent  "Cargo.toml reth-optimism-evm old rev" "^reth-optimism-evm = \{.*rev = \"${cur_opt_rev}\"" "${CARGO_TOML}"
assert_present "Cargo.toml reth-optimism-evm new rev" "^reth-optimism-evm = \{.*rev = \"${OPT_REV}\""     "${CARGO_TOML}"
assert_absent  "Cargo.toml reth-ethereum old rev"     "^reth-ethereum = \{.*rev = \"${cur_reth_rev}\"" "${CARGO_TOML}"
assert_present "Cargo.toml reth-ethereum new rev"     "^reth-ethereum = \{.*rev = \"${RETH_REV}\""     "${CARGO_TOML}"
assert_absent  "Cargo.toml reth-exex-test-utils old"  "^reth-exex-test-utils = \{.*rev = \"${cur_reth_rev}\"" "${CARGO_TOML}"
assert_present "Cargo.toml reth-exex-test-utils new"  "^reth-exex-test-utils = \{.*rev = \"${RETH_REV}\""     "${CARGO_TOML}"
assert_absent  "seed-vendor OPTIMISM_REV old"         "OPTIMISM_REV:-${cur_opt_rev}" "${SEED_SCRIPT}"
assert_present "seed-vendor OPTIMISM_REV new"         "OPTIMISM_REV:-${OPT_REV}"     "${SEED_SCRIPT}"

info "rev rewrites applied + asserted in Cargo.toml and scripts/seed-vendor.sh"

# --- 3. Re-seed the local mirror for the NEW optimism rev --------------------
info "re-seeding local mirror for optimism rev ${OPT_REV} ..."
rm -rf "${REPO_ROOT}/.vendor/optimism"
bash "${SEED_SCRIPT}"

# --- 4. Regenerate / retarget Cargo.lock for the new revs --------------------
# cargo's source replacement blocks `cargo generate-lockfile` ("requires a
# lock file to be present first … remove the source replacement, generate a
# lock file, then restore it"). We follow the documented dance: move the
# generated .cargo/config.toml aside, regenerate the lockfile against the
# stable remote URLs, then restore the replacement config. The regenerated
# Cargo.lock records the canonical remote source strings (portable form).
if [ -f "${CARGO_LOCK}" ]; then
  LOCK_BAK="$(mktemp -t bump-reth-cargolock.XXXXXX)"
  cp "${CARGO_LOCK}" "${LOCK_BAK}"
  info "backed up existing Cargo.lock to ${LOCK_BAK}"
fi

CONFIG_MOVED=0
CONFIG_BAK=""
if [ -f "${CARGO_CONFIG}" ]; then
  CONFIG_BAK="$(mktemp -t bump-reth-cargocfg.XXXXXX)"
  mv "${CARGO_CONFIG}" "${CONFIG_BAK}"
  CONFIG_MOVED=1
  info "moved .cargo/config.toml aside (source replacement disabled for lockfile regen)"
fi

restore_config() {
  if [ "${CONFIG_MOVED}" -eq 1 ] && [ -n "${CONFIG_BAK}" ] && [ -f "${CONFIG_BAK}" ]; then
    mv "${CONFIG_BAK}" "${CARGO_CONFIG}"
    CONFIG_MOVED=0
    info "restored .cargo/config.toml (source replacement re-enabled)"
  fi
}
trap restore_config EXIT

# Drop any stale cargo git cache for these sources so the new revs are fetched.
rm -rf "${HOME}/.cargo/git/db/optimism-"* "${HOME}/.cargo/git/checkouts/optimism-"* 2>/dev/null || true

info "regenerating Cargo.lock against the stable remotes (full clone, slow) ..."
if ! ( cd "${REPO_ROOT}" && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo generate-lockfile ); then
  restore_config
  die "cargo generate-lockfile failed. Cargo.lock left as-is; working tree mutated. Review and re-run; see docs/reth-pin.md 'Bump procedure'."
fi

restore_config
trap - EXIT

# --- 5. Spike co-resolution gate (the fast verification) ---------------------
info "running co-resolution gate: cargo build -p spike --locked ..."
if ! ( cd "${REPO_ROOT}" && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p spike --locked ); then
  cat >&2 <<EOF

bump-reth: ERROR: the spike co-resolution gate FAILED at the new revs.

The working tree has been mutated (Cargo.toml, scripts/seed-vendor.sh,
Cargo.lock, re-seeded .vendor mirror) but NOTHING has been committed.

Likely cause: the two revs are not unified (the paradigmxyz/reth rev does
not match what the optimism monorepo pins), or a transitive semver skew.
See docs/reth-pin.md "Bump procedure" / "Resolution notes".

Do NOT commit. Either fix the revs and re-run, or revert:
  git -C "${REPO_ROOT}" checkout -- Cargo.toml Cargo.lock scripts/seed-vendor.sh
EOF
  exit 1
fi

cat <<EOF

bump-reth: SUCCESS.
  optimism rev: ${cur_opt_rev} -> ${OPT_REV}
  reth     rev: ${cur_reth_rev} -> ${RETH_REV}

Mutated (NOT committed): Cargo.toml, scripts/seed-vendor.sh, Cargo.lock,
re-seeded .vendor mirror (gitignored).

Next:
  1. Review the diff:   git -C "${REPO_ROOT}" diff Cargo.toml Cargo.lock scripts/seed-vendor.sh
  2. Post-bump check:   make verify-pin
  3. Update docs/reth-pin.md version/toolchain/facade tables if anything moved.
  4. Commit yourself (this script never commits).
EOF
