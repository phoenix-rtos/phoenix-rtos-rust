#
# Makefile for umass-rs, the USB Mass Storage driver in Rust
#
# Copyright 2026 Phoenix Systems
#

# Linked with libusb (and libext2, which it mounts partitions with), so built
# where those are: ia32 for now.
# TODO: armv7a7-imx6ull, the other target the C umass runs on
ifeq ($(TARGET_FAMILY),ia32)

# e.g. UMASS_FEATURES="ext2 stack-report" to size the threads' stacks
UMASS_FEATURES ?= ext2

# a USB process driver
NAME := umass-rs
LIBS := usb ext2 cache
LOCAL_CARGO_FLAGS := --features "$(UMASS_FEATURES)"
include $(rust-binary.mk)

# the same driver for the USB server to link (USB_HOSTDRV_LIBS), like the C
# libusbdrv-umass; the server links libusb, libext2 and libcache itself
# (USB_HOSTDRV_LIBS="libusbdrv-umass-rs libext2 libcache")
NAME := libusbdrv-umass-rs
LOCAL_RUST_LIB := umass
LIBS :=
LOCAL_CARGO_FLAGS := --features "$(UMASS_FEATURES) in-server"
include $(rust-static-lib.mk)

endif
