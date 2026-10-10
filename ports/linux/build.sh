#!/bin/bash
# Builds the Linux kernel of Veda's driver VM from the port in this
# directory. `cargo xtask linux` runs it after downloading and verifying the
# sources (see xtask/src/linux.rs and docs/DRIVERVM.md).
#
# Environment:
#   VEDA_PORTS   the ports directory
#   VEDA_LINUX   where to build; downloads/ holds the verified archives
#   JOBS         parallel jobs (default: the number of processors)
#
# Result: $VEDA_LINUX/bzImage.
#
# The kernel's build tool objtool needs libelf, which few machines have the
# headers of: zlib and elfutils' libelf are built for it first, static,
# into build/host-deps.

set -euo pipefail

PORTS=${VEDA_PORTS:?}
ROOT=${VEDA_LINUX:?}
JOBS=${JOBS:-$(nproc)}

DOWNLOADS=$ROOT/downloads
SRC=$ROOT/src
BUILD=$ROOT/build
STAMPS=$BUILD/stamps
DEPS=$BUILD/host-deps

mkdir -p "$SRC" "$BUILD" "$STAMPS"

source "$PORTS/common.sh"

stage_src_zlib() { unpack zlib; }
stage src_zlib "$PORTS/zlib/port.toml"
stage_src_elfutils() { unpack elfutils; }
stage src_elfutils "$PORTS/elfutils/port.toml"
stage_src_linux() { unpack linux; }
stage src_linux "$PORTS/linux/port.toml" "$PORTS/linux/veda.patch"

stage_zlib() {
	local b=$BUILD/zlib
	rm -rf "$b" && cp -r "$SRC/zlib" "$b" && cd "$b"
	quiet configure.log ./configure --static --prefix="$DEPS"
	quiet make.log make -j"$JOBS"
	quiet install.log make install
}
stage zlib src_zlib @DEPS

# A pkg-config that knows only libelf, as built here: elfutils' configure
# insists on finding one, and the kernel asks it for libelf's flags.
pkgconfig() {
	mkdir -p "$DEPS/bin"
	cat > "$DEPS/bin/pkg-config" <<-END
	#!/bin/sh
	for a; do case "\$a" in
	--version) echo 0.29.2; exit 0;;
	--atleast-pkgconfig-version) exit 0;;
	--cflags) echo "-I$DEPS/include"; exit 0;;
	--libs) echo "-L$DEPS/lib -lelf -lz"; exit 0;;
	esac; done
	exit 1
	END
	chmod +x "$DEPS/bin/pkg-config"
}

stage_libelf() {
	local b=$BUILD/elfutils
	pkgconfig
	rm -rf "$b" && mkdir -p "$b" && cd "$b"
	quiet configure.log env PATH="$DEPS/bin:$PATH" "$SRC/elfutils/configure" --prefix="$DEPS" \
		--disable-debuginfod --disable-libdebuginfod --disable-nls --disable-demangler \
		--without-bzlib --without-lzma --without-zstd \
		CPPFLAGS="-I$DEPS/include" LDFLAGS="-L$DEPS/lib"
	quiet make-lib.log make -j"$JOBS" -C lib
	quiet make-libelf.log make -j"$JOBS" -C libelf libelf.a
	cp libelf/libelf.a "$DEPS/lib/"
	cp "$SRC/elfutils/libelf/libelf.h" "$SRC/elfutils/libelf/gelf.h" "$SRC/elfutils/libelf/nlist.h" "$DEPS/include/"
}
stage libelf src_elfutils zlib @DEPS

# Everything off but what the driver VM needs (veda.config).
stage_config() {
	quiet "$BUILD/config.log" make -C "$SRC/linux" O="$BUILD/linux" \
		KCONFIG_ALLCONFIG="$PORTS/linux/veda.config" allnoconfig
}
stage config src_linux "$PORTS/linux/veda.config"

# make knows what changed.
log Building linux
quiet "$BUILD/linux.log" make -C "$SRC/linux" O="$BUILD/linux" HOSTPKG_CONFIG="$DEPS/bin/pkg-config" \
	-j"$JOBS" bzImage
cp "$BUILD/linux/arch/x86/boot/bzImage" "$ROOT/bzImage"
log Finished "Linux $(port linux version) for the driver VM ($ROOT/bzImage)"
