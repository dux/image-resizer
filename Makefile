# make build -> bin/image-resizer-macos-<arch> and bin/image-resizer-linux-x86_64
#
# libvips is linked dynamically, so each binary needs libvips on the machine that runs it:
# Homebrew `vips` on macOS, `libvips42` on Debian 13 (the Linux build runs in a trixie container,
# the distro of the production box). The Linux build needs a running Docker (OrbStack).

NAME := image-resizer
MAC_ARCH := $(shell uname -m)
MAC_BIN := bin/$(NAME)-macos-$(MAC_ARCH)
LINUX_BIN := bin/$(NAME)-linux-x86_64
LINUX_IMAGE := $(NAME)-build:trixie
TARGET_DIR = $(shell cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

help:
	@echo "make build        both binaries into bin/"
	@echo "make build-mac    $(MAC_BIN)"
	@echo "make build-linux  $(LINUX_BIN) (Docker)"
	@echo "make clean        remove the binaries"

build: build-mac build-linux

build-mac:
	cargo build --release --locked
	@mkdir -p bin
	cp "$(TARGET_DIR)/release/$(NAME)" $(MAC_BIN)
	@echo "built $(MAC_BIN)"

# registry and target dir live in named volumes, so repeat builds are incremental
build-linux:
	docker build --platform linux/amd64 -t $(LINUX_IMAGE) -f config/build/Dockerfile.linux config/build
	docker run --rm --platform linux/amd64 \
		-v "$(CURDIR)":/src -w /src \
		-v $(NAME)-cargo-registry:/usr/local/cargo/registry \
		-v $(NAME)-linux-target:/target -e CARGO_TARGET_DIR=/target \
		$(LINUX_IMAGE) \
		sh -c 'cargo build --release --locked && cp /target/release/$(NAME) $(LINUX_BIN)'
	@echo "built $(LINUX_BIN)"

clean:
	rm -f bin/$(NAME)-macos-* bin/$(NAME)-linux-*

.PHONY: help build build-mac build-linux clean
