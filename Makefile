
check:
	cargo check

flash:
	PATH=$(PWD)/picotool/build:${PATH} cargo run --release

fmt:
	cargo +nightly fmt
