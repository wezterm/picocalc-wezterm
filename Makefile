
check:
	cargo +nightly check

clean:
	cargo clean

flash:
	PATH=$(PWD)/picotool/build:${PATH} RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly run --release
	#PATH=$(PWD)/picotool/build:${PATH} cargo +nightly run --release

fmt:
	cargo +nightly fmt
