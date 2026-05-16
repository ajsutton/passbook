#!/usr/bin/env bash
# smoke-dropin.sh — runtime drop-in-safety smoke for a built Passbook image.
#
# Spec clause (validation matrix): "Drop-in safety (no addresses ⇒ stock node;
# namespace absent)". The `node --help` grep in ci.yml only proves the
# `--passbook.addresses` FLAG exists; it never exercises the actual runtime
# property. This script does: it launches the binary as a node and asserts the
# spec'd behaviour at runtime, for whichever image tag is passed.
#
# Usage:
#   bash scripts/smoke-dropin.sh <docker-image-tag>
#
# e.g. `bash scripts/smoke-dropin.sh reth-passbook:ci`
#      `bash scripts/smoke-dropin.sh op-reth-passbook:ci`
#
# It runs the image TWICE (both with `node --dev`, instamine, no peers, HTTP
# RPC up, and `--http.api` deliberately INCLUDING `passbook` so a present
# namespace would always be reachable):
#
#   1. NO `--passbook.addresses`  (drop-in / stock mode):
#        - `eth_chainId` succeeds            ⇒ the stock node is alive.
#        - `passbook_health` returns JSON-RPC error -32601 (method not found)
#                                            ⇒ the `passbook` namespace is
#                                              ABSENT (drop-in safety #1).
#   2. WITH `--passbook.addresses <addr>`  (positive control):
#        - `passbook_health` succeeds        ⇒ proves step 1's absence is the
#                                              real gate, not a misspelled
#                                              method name silently "passing".
#
# Cleanup of both containers is trapped so a failed assertion never leaks a
# running node. Self-contained: needs only docker + curl on the host.

set -euo pipefail

IMAGE="${1:?usage: smoke-dropin.sh <docker-image-tag>}"

# A syntactically valid 20-byte hex address for the positive-control run. The
# value is irrelevant — `--dev` mines empty blocks, nothing is ever watched
# meaningfully; we only need `from_parts` to accept it so the namespace
# registers.
WATCHED_ADDR="0x0000000000000000000000000000000000000001"

CIDS=()
cleanup() {
  for cid in "${CIDS[@]:-}"; do
    [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

# Launch IMAGE as a dev node with HTTP RPC + the `passbook` API explicitly
# allowed, plus any extra args. Echoes the started container id.
start_node() {
  docker run -d --rm -p 0:8545 \
    "$IMAGE" \
    node --dev \
    --http --http.addr 0.0.0.0 --http.port 8545 \
    --http.api eth,net,web3,passbook \
    "$@"
}

# Host port docker mapped to the container's 8545.
host_port() {
  docker port "$1" 8545/tcp | head -n1 | sed 's/.*://'
}

# Poll until `eth_chainId` answers (node + RPC server up) or time out.
wait_for_rpc() {
  local port="$1" _i
  for _i in $(seq 1 90); do
    if curl -fs -m 2 -X POST -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
        "http://127.0.0.1:${port}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "FAIL: RPC never came up on port ${port}" >&2
  return 1
}

# JSON-RPC call → raw response body on stdout.
rpc() {
  local port="$1" method="$2"
  curl -fs -m 5 -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"${method}\",\"params\":[]}" \
    "http://127.0.0.1:${port}"
}

fail() { echo "FAIL: $1" >&2; exit 1; }

echo "== drop-in smoke for ${IMAGE} =="

# ── 1. Stock / drop-in mode: NO watched addresses ───────────────────────────
echo "-- run 1: no --passbook.addresses (expect stock node, namespace absent)"
CID1="$(start_node)"
CIDS+=("$CID1")
PORT1="$(host_port "$CID1")"
wait_for_rpc "$PORT1" || { docker logs "$CID1" >&2 || true; exit 1; }

# Stock node alive: eth_chainId must return a result.
CHAINID_RESP="$(rpc "$PORT1" eth_chainId)"
case "$CHAINID_RESP" in
  *'"result"'*) echo "ok: stock node alive (eth_chainId returned a result)" ;;
  *) fail "stock node did not answer eth_chainId with a result: ${CHAINID_RESP}" ;;
esac

# Drop-in safety: the passbook namespace must be ABSENT. With no addresses the
# namespace is never registered, so an explicitly-allowed `passbook_health`
# call must come back as JSON-RPC "Method not found" (-32601).
HEALTH_RESP="$(rpc "$PORT1" passbook_health)"
case "$HEALTH_RESP" in
  *'"result"'*)
    fail "passbook_health returned a result with NO watched addresses — namespace leaked into a stock node: ${HEALTH_RESP}" ;;
  *-32601*)
    echo "ok: passbook namespace ABSENT (passbook_health ⇒ -32601 method not found)" ;;
  *'"error"'*)
    # Any JSON-RPC error (not a result) still proves the method is not served;
    # accept it but surface the body for diagnosis.
    echo "ok: passbook_health rejected (no result); body: ${HEALTH_RESP}" ;;
  *)
    fail "unexpected passbook_health response with no addresses: ${HEALTH_RESP}" ;;
esac

docker rm -f "$CID1" >/dev/null 2>&1 || true

# ── 2. Positive control: WITH watched addresses ─────────────────────────────
echo "-- run 2: --passbook.addresses ${WATCHED_ADDR} (expect namespace present)"
CID2="$(start_node --passbook.addresses "$WATCHED_ADDR" --passbook.db-path /tmp/passbook-smoke.db)"
CIDS+=("$CID2")
PORT2="$(host_port "$CID2")"
wait_for_rpc "$PORT2" || { docker logs "$CID2" >&2 || true; exit 1; }

HEALTH_RESP2="$(rpc "$PORT2" passbook_health)"
case "$HEALTH_RESP2" in
  *'"result"'*)
    echo "ok: passbook namespace PRESENT when addresses supplied (passbook_health returned a result)" ;;
  *)
    fail "passbook_health did NOT return a result WITH watched addresses — the namespace gate / smoke method name is wrong: ${HEALTH_RESP2}" ;;
esac

docker rm -f "$CID2" >/dev/null 2>&1 || true

echo "== PASS: ${IMAGE} drop-in safety verified at runtime =="
