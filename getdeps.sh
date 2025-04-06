#!/bin/sh
set -xe

brew install libusb pkg-config cmake
git submodule update --init --recursive
rustup target add thumbv8m.main-none-eabihf

# Build picotool, which we need for flashing
export PICO_SDK_PATH=$PWD/pico-sdk
mkdir -p picotool/build
cd picotool/build
cmake -DCMAKE_POLICY_VERSION_MINIMUM=3.5 ..
make -j8

