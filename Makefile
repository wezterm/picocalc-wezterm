
check:
	cargo +nightly check

clean:
	cargo clean

image:
	PATH=$(PWD)/picotool/build:${PATH} RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly build --release
	PATH=$(PWD)/picotool/build:${PATH} picotool uf2 convert -t elf target/thumbv8m.main-none-eabihf/release/picocalc-wezterm wezterm-pico2w.uf2

flash:
	PATH=$(PWD)/picotool/build:${PATH} RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly run --release
	#PATH=$(PWD)/picotool/build:${PATH} cargo +nightly run --release

fmt:
	cargo +nightly fmt
