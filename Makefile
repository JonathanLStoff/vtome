# Build, test, and run vtome.
#
#   make            list every target
#   make test       run the test suite
#   make show       put a picture on a monitor
#
# vtome is a library with no binary. Add example targets here as they appear,
# one each, so a target can carry the feature its example needs.

CARGO ?= cargo

# --- `make show` / `make identify` ----------------------------------------
# The file to show or identify.
FILE ?=
# Which monitor: an index, or part of its name. `make monitors` lists them.
MONITOR ?=
# Corner-pin the picture into a trapezoid, narrowing the top edge by this
# fraction of the width at each end (0.0 to 0.49). The keystone a projector
# makes when it is aimed upwards. A fraction rather than pixels, so it does not
# need to know how big the monitor is first.
KEYSTONE ?=
# 0.0 to 1.0.
OPACITY ?=

# Debug by default. Override when timing anything that touches pixels, since a
# debug build measures the wrong thing:  make show PROFILE=--release
PROFILE ?=

# The version to cut: make release v=0.2.0
v ?=

.DEFAULT_GOAL := help
.PHONY: help build test check fmt clippy doc clean release \
        show monitors identify corner-pin require-cargo require-manifest \
        require-file require-version

# --- checks ---------------------------------------------------------------

require-cargo:
	@command -v $(CARGO) >/dev/null 2>&1 || { \
		echo "error: $(CARGO) not found. Install Rust from https://rustup.rs"; \
		exit 1; }

require-manifest: require-cargo
	@$(CARGO) metadata --no-deps --format-version 1 >/dev/null 2>&1 || { \
		echo "error: Cargo.toml does not parse. Run '$(CARGO) metadata' to see why."; \
		exit 1; }

# The examples that take a file should say so before cargo builds for a minute
# and then says it itself.
require-file:
	@test -n "$(FILE)" || { \
		echo "error: no file. Usage: make $(MAKECMDGOALS) FILE=poster.png"; \
		exit 1; }
	@test -f "$(FILE)" || { \
		echo "error: $(FILE) does not exist."; \
		exit 1; }

require-version:
	@test -n "$(v)" || { \
		echo "error: no version. Usage: make release v=0.2.0"; \
		exit 1; }
	@printf '%s' '$(v)' \
		| grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$$' || { \
		echo "error: '$(v)' is not a semver version (x.y.z)"; \
		exit 1; }

# --- examples -------------------------------------------------------------

## show: put a picture on a monitor (make show FILE=x.png MONITOR=1 KEYSTONE=0.15)
show: require-manifest require-file
	@MONITOR=$(MONITOR) KEYSTONE=$(KEYSTONE) OPACITY=$(OPACITY) \
		$(CARGO) run $(PROFILE) --features window,image --example show -- "$(FILE)"

## monitors: list the monitors vtome can place a window on
monitors: require-manifest
	@LIST=1 $(CARGO) run $(PROFILE) --quiet --features window,image --example show

## identify: say what a file is, and what is inside it (make identify FILE=clip.mp4)
identify: require-manifest require-file
	$(CARGO) run $(PROFILE) --example identify -- "$(FILE)"

## corner-pin: print the trapezoid maths, and how far wrong the naive version is
corner-pin: require-manifest
	$(CARGO) run $(PROFILE) --quiet --example corner_pin

# --- build and test -------------------------------------------------------

## build: compile the crate
build: require-manifest
	$(CARGO) build $(PROFILE)

## test: run the test suite, including the GPU tests
test: require-manifest
	$(CARGO) test --features render

## test-core: the tests that need no GPU and no optional dependency
test-core: require-manifest
	$(CARGO) test --no-default-features

## check: compile-check every feature combination that has to keep working
check: require-manifest
	$(CARGO) check --no-default-features
	$(CARGO) check
	$(CARGO) check --features render
	$(CARGO) check --features window --all-targets

## fmt: format the source
fmt: require-cargo
	$(CARGO) fmt

## clippy: lint, with warnings as errors
clippy: require-manifest
	$(CARGO) clippy --features window,image --all-targets -- -D warnings

## doc: build the documentation
doc: require-manifest
	$(CARGO) doc --no-deps --features window,image

## clean: remove build artefacts
clean:
	$(CARGO) clean

# --- release --------------------------------------------------------------

## release: set the version everywhere (make release v=0.2.0)
release: require-version require-manifest
	@awk -v ver='$(v)' ' \
		/^\[/ { in_pkg = ($$0 == "[package]") } \
		in_pkg && !done && /^version[[:space:]]*=/ { \
			print "version = \"" ver "\""; done = 1; next } \
		{ print } \
	' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
	@grep -Fqx 'version = "$(v)"' Cargo.toml || { \
		echo "error: Cargo.toml still does not say $(v)."; exit 1; }
	@sed -i.bak -E 's/^vtome = \{ version = "[^"]*"/vtome = { version = "$(v)"/' \
		README.md && rm -f README.md.bak
	@$(CARGO) metadata --format-version 1 >/dev/null
	@echo "==> $(v): Cargo.toml, Cargo.lock, README.md"
	@echo "    Review the diff, then commit."

# --- help -----------------------------------------------------------------

help:
	@echo "vtome — Video Translucent Optimized MacGyver Engine"
	@echo ""
	@grep -E '^## ' $(MAKEFILE_LIST) \
		| sed -e 's/^## //' -e 's/:/:|/' \
		| awk -F'|' '{ printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 }'
	@echo ""
	@echo "  Put an image or a video on a specific monitor — or on a specific"
	@echo "  quadrilateral of one. See planning/TODO.md for what is left."
	@echo ""
	@echo "    make monitors"
	@echo "    make show FILE=poster.png MONITOR=1 KEYSTONE=0.15"
	@echo "    make identify FILE=clip.mp4"
