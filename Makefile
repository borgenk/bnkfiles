.PHONY: fmt fmt-check clippy build build-release build-linux install run run-release test \
	test-install test-abi check perf perf-save perf-lab tables shaders test-ui bump clean

APP_NAME := bnkfiles
APP_ID := io.github.borgenk.BnkFiles
BUILD_PATH := target/release
VERSION := $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
LINUX_TARGET := x86_64-unknown-linux-gnu
BIN_DIR := ~/.local/bin
APPS_DIR := ~/.local/share/applications

# The oldest glibc a downloaded tarball runs on, which is Debian 12's. The
# release builds in that image and checks the binary against this.
GLIBC_FLOOR := 2.36

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --all --benches --tests --examples --all-features -- -D warnings

build:
	cargo build

build-release:
	cargo build --release
	@ls -lh $(BUILD_PATH)/$(APP_NAME)

# Install the release binary and desktop entry under ~/.local, the same
# per-user location install.sh uses. The desktop database refresh is
# best-effort (ignored if the tool is absent).
install: build-release
	install -Dm755 $(BUILD_PATH)/$(APP_NAME) $(BIN_DIR)/$(APP_NAME)
	install -Dm644 assets/$(APP_ID).desktop $(APPS_DIR)/$(APP_ID).desktop
	-update-desktop-database $(APPS_DIR)
	@echo "Installed $(APP_NAME) to ~/.local/bin/"
	@echo "Installed desktop entry to ~/.local/share/applications/"

# Bump version, commit, and tag: make bump V=0.2.0
# The release workflow rejects a tag that does not match this version.
bump:
	@test -n "$(V)" || (echo "Current: $(VERSION). Usage: make bump V=0.2.0" && exit 1)
	sed -i '0,/^version = ".*"/{s//version = "$(V)"/}' Cargo.toml
	cargo update --workspace
	git add Cargo.toml Cargo.lock
	git commit -m "Bump version to $(V)"
	git tag "v$(V)"
	@echo "Bumped to v$(V). Push with: git push origin main --tags"

# Build a release tarball into dist/ for upload to a GitHub Release, with the
# checksum install.sh verifies beside it. Bundles the binary and desktop entry
# so install.sh can place them.
build-linux: TARBALL := $(APP_NAME)-v$(VERSION)-$(LINUX_TARGET).tar.gz
build-linux:
	RUSTFLAGS="--remap-path-prefix=$(HOME)=[home]" \
		cargo build --release --target $(LINUX_TARGET)
	sh .github/scripts/check-abi.sh $(GLIBC_FLOOR) target/$(LINUX_TARGET)/release/$(APP_NAME)
	rm -rf dist/stage
	mkdir -p dist/stage
	cp target/$(LINUX_TARGET)/release/$(APP_NAME) dist/stage/$(APP_NAME)
	cp assets/$(APP_ID).desktop dist/stage/$(APP_ID).desktop
	tar czf dist/$(TARBALL) -C dist/stage .
	rm -rf dist/stage
	cd dist && sha256sum $(TARBALL) > $(TARBALL).sha256
	@echo "Built dist/$(TARBALL), with a .sha256"
	@echo "Publish with: gh release create v$(VERSION) dist/$(TARBALL)*"

# Run with an optional image/folder: `make run ARGS=~/Pictures`.
run:
	cargo run -- $(ARGS)

# Optimized run; the decoders and the thumbnail downscaler are several times
# faster with optimizations, so use this to judge real performance:
#   make run-release ARGS=~/Pictures
run-release:
	cargo run --release -- $(ARGS)

test:
	cargo test

# The installer, over file:// URLs, with no network.
test-install:
	sh .github/scripts/test-install.sh

# The ABI floor check, against binaries this machine already has.
test-abi:
	sh .github/scripts/test-abi.sh

# Run the offline performance gate (src/dev/perf.rs) with the counting allocator
# on, so it reports allocations per operation. Always release: debug numbers are
# meaningless. Exits nonzero if a scenario regressed past the gate's threshold.
perf:
	cargo run --release --features perf-alloc -- --perf

# Accept the current perf numbers as the new baseline (perf/baseline.txt).
perf-save:
	cargo run --release --features perf-alloc -- --perf --save

# Run the end-to-end pipeline lab (src/dev/lab.rs): grid-fill and large-image
# timings against the real decode pipeline, vs perf/lab-baseline.txt. Pass ARGS=--save
# to accept new numbers. No counting allocator: it measures wall-clock across threads.
perf-lab:
	cargo run --release -- --perf-lab $(ARGS)

# Regenerate src/platform/grapheme_tables.rs from the Unicode data vendored in
# ucd/. The only way to do it on a Unicode bump.
tables:
	cargo run --release -- --gen-tables

# Recompile the committed SPIR-V after editing a shader. Needs glslc (shaderc)
# and spirv-val (spirv-tools); a normal build needs neither.
shaders:
	glslc -O shaders/quad.vert -o shaders/quad.vert.spv
	glslc -O shaders/quad.frag -o shaders/quad.frag.spv
	spirv-val shaders/quad.vert.spv
	spirv-val shaders/quad.frag.spv

# Drive the real binary inside a headless compositor (tests/ui.rs): key
# presses, clicks, screenshots. Needs labwc, grim and a GPU, so it is not part
# of `check`.
test-ui:
	cargo test --test ui -- --ignored

check: fmt clippy test test-install test-abi

clean:
	cargo clean
