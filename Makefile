# Passbook — common targets.
#
# All cargo builds are `--locked` (Cargo.lock is committed + load-bearing) and
# set CARGO_NET_GIT_FETCH_WITH_CLI=true (the system git binary clones the
# shallow op-reth mirror correctly; cargo's libgit2 mishandles shallow clones).
#
# Bootstrap on any fresh machine / CI / Docker stage: `make seed` once, then
# build. See docs/reth-pin.md.
#
# Usage:
#   make build          workspace build (--locked)
#   make test           workspace test  (--locked)
#   make seed           (re)create the gitignored .vendor mirror + .cargo config
#   make verify-pin     seed + spike co-resolution gate (post-bump check)
#   make bump ARGS='--optimism-rev <SHA> --reth-rev <SHA>'
#                       lockstep reth/op-reth rev bump (does NOT commit)
#   make docker         build the two binary images (needs Dockerfile, Task 9.1)
#   make help           this message

CARGO_ENV := CARGO_NET_GIT_FETCH_WITH_CLI=true

.PHONY: help build test seed verify-pin bump docker

help:
	@echo 'Passbook targets:'
	@echo '  make build          workspace build (--locked)'
	@echo '  make test           workspace test  (--locked)'
	@echo '  make seed           (re)create gitignored .vendor mirror + .cargo config'
	@echo '  make verify-pin     seed + spike co-resolution gate (post-bump check)'
	@echo "  make bump ARGS='--optimism-rev <SHA> --reth-rev <SHA>'"
	@echo '                      lockstep reth/op-reth rev bump (does NOT commit)'
	@echo '  make docker         build the two binary images (needs Dockerfile, Task 9.1)'
	@echo '  make help           this message'

build:
	$(CARGO_ENV) cargo build --workspace --locked

test:
	$(CARGO_ENV) cargo test --workspace --locked

seed:
	bash scripts/seed-vendor.sh

# Post-bump correctness check: regenerate the gitignored mirror/config, run the
# spike co-resolution gate, then the full passbook-core suite (unit +
# `exex_integration` integration tests) so a rev bump is validated end-to-end.
verify-pin:
	bash scripts/seed-vendor.sh && $(CARGO_ENV) cargo build -p spike --locked && $(CARGO_ENV) cargo test -p passbook-core --locked

# Lockstep reth/op-reth rev bump. Pass both revs via ARGS, e.g.:
#   make bump ARGS='--optimism-rev <opt-sha> --reth-rev <reth-sha>'
# The script updates both revs everywhere, re-seeds the mirror, regenerates
# Cargo.lock, runs the spike gate, and does NOT commit (operator reviews).
bump:
	bash scripts/bump-reth.sh $(ARGS)

# The Dockerfile lands in Task 9.1; these targets will not work until then.
docker:
	docker build -t reth-passbook:dev --target reth-passbook . && \
	docker build -t op-reth-passbook:dev --target op-reth-passbook .
