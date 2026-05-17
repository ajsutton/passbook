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
#   make docker         build the two binary images locally (tags :dev)
#   make docker-publish build + push images to GHCR (release profile OOMs
#                       free CI, so images are built/published locally)
#   make help           this message

CARGO_ENV := CARGO_NET_GIT_FETCH_WITH_CLI=true

# Image publishing (override on the command line as needed):
#   REGISTRY/IMAGE_OWNER → ghcr.io/<owner>/{reth,op-reth}-passbook
#   VERSION set    → push exactly that tag (release, e.g. VERSION=v1.2.3)
#   VERSION unset  → push the short commit SHA + a moving `latest`
# Requires `docker login ghcr.io` first (a token/PAT with write:packages).
REGISTRY    ?= ghcr.io
IMAGE_OWNER ?= ajsutton
GIT_SHA     := $(shell git rev-parse --short HEAD)
VERSION     ?=

.PHONY: help build test seed verify-pin bump docker docker-publish

help:
	@echo 'Passbook targets:'
	@echo '  make build          workspace build (--locked)'
	@echo '  make test           workspace test  (--locked)'
	@echo '  make seed           (re)create gitignored .vendor mirror + .cargo config'
	@echo '  make verify-pin     seed + spike co-resolution gate (post-bump check)'
	@echo "  make bump ARGS='--optimism-rev <SHA> --reth-rev <SHA>'"
	@echo '                      lockstep reth/op-reth rev bump (does NOT commit)'
	@echo '  make docker         build the two binary images locally (tags :dev)'
	@echo '  make docker-publish build + push images to GHCR (local, not CI)'
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

docker:
	docker build -t reth-passbook:dev --target reth-passbook . && \
	docker build -t op-reth-passbook:dev --target op-reth-passbook .

# Build (via the `docker` target) then tag + push both images to GHCR.
# The release profile (codegen-units=1 + thin-LTO) peaks well above free-CI
# RAM, so publishing is a local operator step, not a CI job.
docker-publish: docker
	@set -eu; \
	owner=$$(printf '%s' '$(IMAGE_OWNER)' | tr 'A-Z' 'a-z'); \
	if [ -n '$(VERSION)' ]; then tags='$(VERSION)'; else tags='$(GIT_SHA) latest'; fi; \
	echo "Publishing to $(REGISTRY)/$$owner — tags: $$tags"; \
	for bin in reth-passbook op-reth-passbook; do \
	  for t in $$tags; do \
	    docker tag $$bin:dev $(REGISTRY)/$$owner/$$bin:$$t; \
	    docker push $(REGISTRY)/$$owner/$$bin:$$t; \
	    echo "pushed $(REGISTRY)/$$owner/$$bin:$$t"; \
	  done; \
	done
