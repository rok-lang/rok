# The image dist-linux-gnu.sh runs dist-native.sh in: Debian 11 (glibc 2.31), so
# what it builds needs at most the glibc 2.34 dist-native.sh allows.
#
# Debian's packages come from one snapshot; bullseye-security carries
# clang 22.1.8, the pinned LLVM's, which reads llvm/rok.profdata.xz.
# snapshot.debian.org drops requests under load, so apt retries each
# download. cmake is Kitware's (Debian 11's is older than LLVM's
# minimum). Rust comes from rust-toolchain.toml through rustup-init, into
# the cache.
FROM docker.io/library/debian:bullseye@sha256:6f519a81440354a85eb592c5f32109ab80605f6b892455983a6f618bf87fabe9

ARG SNAPSHOT=20260901T000000Z
ARG CMAKE=3.31.12
ARG CMAKE_SHA256_X86_64=0dc2e9a6860f06bf10bd8fadc03e35d9eeb4df46e33763a7e480e987758f385c
ARG CMAKE_SHA256_AARCH64=83f8fd91d2038a56556e1400390fcfe42f79602940c494f6c6f1cdae7f9e7f40
ARG RUSTUP=1.28.2
ARG RUSTUP_SHA256_X86_64=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
ARG RUSTUP_SHA256_AARCH64=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c

RUN s="deb [check-valid-until=no] http://snapshot.debian.org/archive" && \
	printf '%s\n' "$s/debian/$SNAPSHOT bullseye main" "$s/debian/$SNAPSHOT bullseye-updates main" \
		"$s/debian-security/$SNAPSHOT bullseye-security main" >/etc/apt/sources.list && \
	printf '%s\n' 'Acquire::Retries "10";' 'Acquire::http::Timeout "120";' >/etc/apt/apt.conf.d/80retries && \
	apt-get update && \
	DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
		ca-certificates curl binutils gcc g++ make patch xz-utils python3 git ninja-build \
		clang-22=1:22.1.8-1~deb11u1 && \
	ln -s /usr/bin/clang-22 /usr/local/bin/clang && \
	ln -s /usr/bin/clang++-22 /usr/local/bin/clang++ && \
	rm -rf /var/lib/apt/lists/*

RUN set -e; \
	case $(uname -m) in \
	x86_64) c=$CMAKE_SHA256_X86_64 r=$RUSTUP_SHA256_X86_64 ;; \
	aarch64) c=$CMAKE_SHA256_AARCH64 r=$RUSTUP_SHA256_AARCH64 ;; \
	esac; \
	n=cmake-$CMAKE-linux-$(uname -m); \
	curl -fsSL -o /tmp/cmake.tar.gz "https://github.com/Kitware/CMake/releases/download/v$CMAKE/$n.tar.gz"; \
	echo "$c  /tmp/cmake.tar.gz" | sha256sum -c -; \
	tar -xzf /tmp/cmake.tar.gz -C /opt; \
	ln -s "/opt/$n/bin/cmake" /usr/local/bin/cmake; \
	rm /tmp/cmake.tar.gz; \
	curl -fsSL -o /usr/local/bin/rustup-init \
		"https://static.rust-lang.org/rustup/archive/$RUSTUP/$(uname -m)-unknown-linux-gnu/rustup-init"; \
	echo "$r  /usr/local/bin/rustup-init" | sha256sum -c -; \
	chmod +x /usr/local/bin/rustup-init

ENV CC=clang CXX=clang++
