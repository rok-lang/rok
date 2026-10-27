# The image dist-linux-musl.sh runs dist-native.sh in: Alpine (musl), so rok, rokc,
# rokls and librokr.a are built against musl and the archive is fully
# static. Packages come from the pinned base image's release; Rust is the
# pinned toolchain, installed by dist-linux-musl.sh through rustup-init.
FROM docker.io/library/alpine:3.21@sha256:ee16d92f96db08d9d2d07da48555a27f3a69d8f0759021dbc601195ed0d2b5bd

ARG RUSTUP=1.28.2
ARG RUSTUP_SHA256_X86_64=e6599a1c7be58a2d8eaca66a80e0dc006d87bbcf780a58b7343d6e14c1605cb2
ARG RUSTUP_SHA256_AARCH64=a97c8f56d7462908695348dd8c71ea6740c138ce303715793a690503a94fc9a9

# llvm/build.sh: cmake, ninja, g++, curl, xz, patch, linux-headers.
# targets/build.sh: clang, lld, make, python3. dist-native.sh: git, tar, and
# strip (binutils, in build-base). Real ninja from pip, not Alpine's samurai:
# samurai races the tablegen outputs and leaves llvm/.../TargetLibraryInfo.inc
# missing when a dependent compiles.
RUN apk add --no-cache \
	build-base clang lld cmake curl xz patch linux-headers \
	python3 py3-pip git bash tar ca-certificates && \
	pip install --no-cache-dir --break-system-packages ninja

RUN set -e; \
	case $(uname -m) in \
	x86_64) r=$RUSTUP_SHA256_X86_64 ;; \
	aarch64) r=$RUSTUP_SHA256_AARCH64 ;; \
	esac; \
	curl -fsSL -o /usr/local/bin/rustup-init \
		"https://static.rust-lang.org/rustup/archive/$RUSTUP/$(uname -m)-unknown-linux-musl/rustup-init"; \
	echo "$r  /usr/local/bin/rustup-init" | sha256sum -c -; \
	chmod +x /usr/local/bin/rustup-init
