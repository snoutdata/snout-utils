#!/usr/bin/env bash
# Builds snout_utils for one Postgres major, in two steps so a container build can cache the first:
#
#   bash scripts/build-dist.sh tools 17            # the pgdg server headers and libclang
#   bash scripts/build-dist.sh build 17 /out       # <out>/lib/snout_utils.so, stripped
#                                                  # <out>/debug/snout_utils.so.debug, its symbols
#
# The one recipe for a build that ships: the SnoutData Cloud pod image runs both steps in a build
# stage with this folder as a named build context, so the image depends on this package's output
# and never the reverse. It expects Debian bookworm with the Rust toolchain Cargo.toml's
# rust-version names.
#
# Only the library is shipped. snout_utils has no SQL objects, so there is no control file or
# script to install, and it is not something a database can CREATE EXTENSION.
set -euo pipefail

step="${1:?usage: scripts/build-dist.sh tools <major> | build <major> <out dir>}"
major="${2:?the Postgres major}"
src="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "$major" in
	17) ;;
	*) echo "snout_utils is built and tested for Postgres 17, not $major" >&2; exit 2 ;;
esac

pg_config="/usr/lib/postgresql/${major}/bin/pg_config"

if [ "$step" = tools ]; then
	# A different rustc builds a different binary from the same source, and nothing downstream
	# would notice.
	want="$(tr -d '\r' <"$src/Cargo.toml" | sed -n 's/^rust-version *= *"\(.*\)"/\1/p')"
	have="$(rustc --version | awk '{print $2}')"
	if [ "$want" != "$have" ]; then
		echo "Cargo.toml pins rust $want; this image has rustc $have" >&2
		exit 1
	fi

	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y --no-install-recommends build-essential clang libclang-dev pkg-config ca-certificates curl gnupg
	install -d /usr/share/postgresql-common/pgdg
	curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc
	echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
		>/etc/apt/sources.list.d/pgdg.list
	apt-get update
	apt-get install -y --no-install-recommends "postgresql-server-dev-${major}"
	rm -rf /var/lib/apt/lists/*
	exit 0
fi

[ "$step" = build ] || { echo "unknown step: $step" >&2; exit 2; }
out="${3:?the out dir}"

# A copy to build in, so the build context stays read-only.
#
# A plain `cargo build`, not `cargo pgrx install`: this library has no SQL objects, so there is no
# schema to generate, and pgrx's schema generation is what keeps most of a pgrx library's exported
# symbols alive (2.9 MB stripped that way, 0.7 MB this way). pgrx finds the server's headers
# through PGRX_PG_CONFIG_PATH, so cargo-pgrx is not needed at all.
work="$(mktemp -d)"
cp -r "$src/Cargo.toml" "$src/Cargo.lock" "$src/src" "$work/"
export PGRX_PG_CONFIG_PATH="$pg_config"
(cd "$work" && cargo build --release --locked --lib --no-default-features --features "pg${major}")

mkdir -p "$out/lib" "$out/debug"
so="${CARGO_TARGET_DIR:-$work/target}/release/libsnout_utils.so"
objcopy --only-keep-debug "$so" "$out/debug/snout_utils.so.debug"
strip --strip-unneeded -o "$out/lib/snout_utils.so" "$so"
ls -l "$out/lib"
