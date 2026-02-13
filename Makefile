CHIP=pico2w
#pimoroni2w

#FEATURES=${CHIP},configsdcard
FEATURES=${CHIP}


check:
	cargo +nightly check --features $(FEATURES)
clean:
	cargo clean
	rm *.uf2

image:
	PATH="$(PWD)/picotool/build:${PATH}" RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly build --release --features $(FEATURES)
	PATH="$(PWD)/picotool/build:${PATH}" picotool uf2 convert -t elf target/thumbv8m.main-none-eabihf/release/picocalc-wezterm wezterm-$(CHIP)-`git -c core.abbrev=8 show -s --format=%cd-%h --date=format:%Y%m%d`.uf2

flash-ocd: image
	PATH="$(PWD)/picotool/build:${PATH}" RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly build --release --features $(FEATURES)
	openocd -f interface/cmsis-dap.cfg -f target/rp2350.cfg -c "adapter speed 5000" -c "program target/thumbv8m.main-none-eabihf/release/picocalc-wezterm verify reset exit"

flash:
	PATH="$(PWD)/picotool/build:${PATH}" RUST_LOG=info RUSTC_LOG=rustc_codegen_ssa::back::link=info cargo +nightly run --release --features $(FEATURES)
	#PATH="$(PWD)/picotool/build:${PATH}" cargo +nightly run --release

fmt:
	cargo +nightly fmt
