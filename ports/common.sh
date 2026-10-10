# The helpers the build scripts of the ports share (`build.sh`, and
# `linux/build.sh`). They expect PORTS (the ports directory), DOWNLOADS
# (the verified archives), SRC (where ports are unpacked) and STAMPS
# (the stages' fingerprints).

log() { printf '\033[1;32m%12s\033[0m %s\n' "$1" "$2"; }

# The value of KEY in ports/NAME/port.toml.
port() {
	sed -n "s/^$2 *= *\"\(.*\)\"/\1/p" "$PORTS/$1/port.toml"
}

# Runs stage NAME (a shell function stage_NAME) unless its fingerprint is
# unchanged. Other arguments are what it depends on: files, stages, and
# variables it uses (@NAME).
stage() {
	local name=$1
	shift
	local print
	print=$({
		declare -f "stage_$name"
		for dep in "$@"; do
			case "$dep" in
			@*) declare -p "${dep#@}" ;;
			*) if [ -f "$dep" ]; then sha256sum "$dep"; else cat "$STAMPS/$dep"; fi ;;
			esac
		done
	} | sha256sum | cut -d' ' -f1)
	if [ -f "$STAMPS/$name" ] && [ "$(cat "$STAMPS/$name")" = "$print" ]; then
		log Fresh "$name"
		return
	fi
	log Building "$name"
	rm -f "$STAMPS/$name"
	"stage_$name"
	echo "$print" > "$STAMPS/$name"
}

# Runs a command with its output in LOG, showing the end of it on failure.
quiet() {
	local logfile=$1
	shift
	if ! "$@" > "$logfile" 2>&1; then
		tail -n 40 "$logfile"
		echo "failed: $* (log: $logfile)" >&2
		exit 1
	fi
}

# Unpacks port NAME into $SRC/NAME and applies its patch, if it has one;
# with `upstream`, as its authors released it (the patch is for Veda).
unpack() {
	local name=$1 version archive dir
	version=$(port "$name" version)
	archive=$DOWNLOADS/$(basename "$(port "$name" url)")
	dir=$SRC/$name
	rm -rf "$dir" "$dir.tmp"
	mkdir -p "$dir.tmp"
	tar -xf "$archive" -C "$dir.tmp"
	mv "$dir.tmp/$name-$version" "$dir"
	rmdir "$dir.tmp"
	if [ "${2:-}" != upstream ] && [ -f "$PORTS/$name/veda.patch" ]; then
		(cd "$dir" && patch -p1 --batch --forward --quiet < "$PORTS/$name/veda.patch")
	fi
}
