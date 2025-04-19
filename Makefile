
check:
	cargo +nightly check

clean:
	cargo clean
	rm *.uf2

image:
	PATH=$(PWD)/picotool/build:${PATH} RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly build --release
	PATH=$(PWD)/picotool/build:${PATH} picotool uf2 convert -t elf target/thumbv8m.main-none-eabihf/release/picocalc-wezterm wezterm-pico2w-`git -c core.abbrev=8 show -s --format=%cd-%h --date=format:%Y%m%d`.uf2

flash:
	PATH=$(PWD)/picotool/build:${PATH} RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly run --release
	#PATH=$(PWD)/picotool/build:${PATH} cargo +nightly run --release

fmt:
	cargo +nightly fmt
