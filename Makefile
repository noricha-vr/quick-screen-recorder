.PHONY: check fmt clippy test test-scripts build bundle install clean

check: fmt clippy test test-scripts

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets --locked -- -D warnings

test:
	cargo test --all-targets --locked

test-scripts:
	bash scripts/test-install-local.sh

build:
	RUSTFLAGS="$${RUSTFLAGS:-} --remap-path-prefix=$${CARGO_HOME:-$$HOME/.cargo}=/cargo --remap-path-prefix=$$HOME=~ --remap-path-prefix=$$(pwd)=." cargo build --release --locked

bundle:
	bash scripts/bundle-app.sh

install:
	bash scripts/install-local.sh

clean:
	cargo clean
	rm -rf dist
