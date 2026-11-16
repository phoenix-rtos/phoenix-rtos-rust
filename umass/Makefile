#
# Makefile for umass-rs, the USB Mass Storage driver in Rust
#
# Copyright 2026 Phoenix Systems
#

# A USB process driver, linked with libusb (and libext2, which it mounts
# partitions with), so built where those are: ia32 for now.
# TODO: armv7a7-imx6ull, the other target the C umass runs on
ifeq ($(TARGET_FAMILY),ia32)

NAME := umass-rs
LIBS := usb ext2
# e.g. UMASS_FEATURES="ext2 stack-report" to size the threads' stacks
UMASS_FEATURES ?= ext2
LOCAL_CARGO_FLAGS := --features "$(UMASS_FEATURES)"

include $(rust-binary.mk)

endif
