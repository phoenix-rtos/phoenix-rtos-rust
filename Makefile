#
# Makefile for phoenix-rtos-rust
#
# Copyright 2026 Phoenix Systems
#
# %LICENSE%
#

include ../phoenix-rtos-build/Makefile.common

.DEFAULT_GOAL := all

# default path for the programs to be installed in rootfs
DEFAULT_INSTALL_PATH := /sbin

# read out all components; libraries are built as dependencies of the
# components using them and have a Makefile (rust-test.mk) only for their tests
ALL_MAKES := $(wildcard */Makefile)
include $(ALL_MAKES)

DEFAULT_COMPONENTS := $(ALL_COMPONENTS)

# Rust components are only in ALL_COMPONENTS with RUST_ENABLE=y (see
# phoenix-rtos-build/makes/setup-rust.mk); skip targets Rust has no port for.
ifeq ($(RUST_TARGET),)
  $(info phoenix-rtos-rust: no Rust port for TARGET=$(TARGET), skipping)
  DEFAULT_COMPONENTS :=
  DEFAULT_TEST_COMPONENTS :=
endif

# create generic targets
.PHONY: all install clean
all: $(DEFAULT_COMPONENTS)
install: $(patsubst %,%-install,$(DEFAULT_COMPONENTS))
clean: $(patsubst %,%-clean,$(ALL_COMPONENTS))
