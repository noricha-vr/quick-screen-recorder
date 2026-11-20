.PHONY: check fmt clippy test build bundle clean

check: fmt clippy test

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets --locked -- -D warnings

test:
	cargo test --all-targets --locked

build:
	RUSTFLAGS="$${RUSTFLAGS:-} --remap-path-prefix=$${CARGO_HOME:-$$HOME/.cargo}=/cargo --remap-path-prefix=$$HOME=~ --remap-path-prefix=$$(pwd)=." cargo build --release --locked

bundle:
	bash scripts/bundle-app.sh

clean:
	cargo clean
	rm -rf dist
