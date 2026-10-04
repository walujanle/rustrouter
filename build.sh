#!/usr/bin/env bash
#
# rustrouter production build for Linux (and macOS).
#
# Order matters: crates/router-server embeds web/dist through rust-embed
# (`#[folder = "../../web/dist"]`), which compiles each asset in with
# include_bytes!. The backend is therefore built AFTER the frontend, or the
# binary carries a stale (or missing) dashboard.
#
# Memory: the release profile sets codegen-units = 1 and thin LTO. Codegen is
# rustc's only default parallelism (one thread per codegen unit), so
# codegen-units = 1 already keeps a single rustc small - the lever that matters
# is how many crates build at once. This script sizes the job count to the
# memory actually available (leaving a reserve for the OS and any running
# rustrouter) and, when memory is tight, drops LTO so the cross-crate merge
# step does not run.
#
# Output: rustrouter-binary/rustrouter
#
#   ./build.sh                     auto-size jobs to available RAM
#   ./build.sh --low-mem           force the low-memory profile and -j1
#   ./build.sh --jobs 2            cap parallelism explicitly
#   ./build.sh --release-name      also emit the CI asset name (rustrouter-linux-amd64-0.1.0)
#   ./build.sh --no-frontend       reuse an existing web/dist (backend only)
#   ./build.sh --self-test         run the job-sizing self-check and exit
#
# Knobs: RUSTROUTER_MEM_PER_JOB_MB (default 1800), RUSTROUTER_MEM_RESERVE_MB
# (default 1024).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB="$ROOT/web"
OUT="$ROOT/rustrouter-binary"

PER_JOB_MB="${RUSTROUTER_MEM_PER_JOB_MB:-1800}"
RESERVE_MB="${RUSTROUTER_MEM_RESERVE_MB:-1024}"

LOW_MEM=0
JOBS=""
BUILD_FRONTEND=1
RELEASE_NAME=0
TARGET=""
SELF_TEST=0

# ─── memory sizing (pure, so it can be self-tested) ───────────────────────

# jobs to run given the memory available and the CPU count. Never below 1: a
# single job that does not fit is still attempted, with the low-memory profile.
compute_jobs() { # <available_mb> <cpus> <per_job_mb> <reserve_mb>
	awk -v avail="$1" -v cpus="$2" -v per="$3" -v reserve="$4" 'BEGIN {
		budget = avail - reserve
		if (budget < per) { print 1; exit }
		jobs = int(budget / per)
		if (jobs > cpus) jobs = cpus
		if (jobs < 1) jobs = 1
		print jobs
	}'
}

mem_available_mb() {
	if [ -r /proc/meminfo ]; then
		awk '/^MemAvailable:/ { a = $2 } /^MemTotal:/ { t = $2 }
			END { if (a > 0) print int(a / 1024); else print int(t / 1024) }' /proc/meminfo
	elif command -v sysctl >/dev/null 2>&1; then
		local bytes
		bytes="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
		[ "$bytes" -gt 0 ] && echo $((bytes / 1024 / 1024)) || echo 0
	else
		echo 0
	fi
}

cpu_count() {
	if command -v nproc >/dev/null 2>&1; then
		nproc
	elif command -v sysctl >/dev/null 2>&1; then
		sysctl -n hw.ncpu 2>/dev/null || echo 1
	else
		echo 1
	fi
}

# The asset prefix CI uses, so a manual build matches the published name.
platform_slug() {
	local os arch
	os="$(uname -s)"
	arch="$(uname -m)"
	case "$os" in
	Linux) os="linux" ;;
	Darwin) os="macos" ;;
	*) os="$(echo "$os" | tr '[:upper:]' '[:lower:]')" ;;
	esac
	case "$arch" in
	x86_64 | amd64) arch="amd64" ;;
	aarch64 | arm64) arch="arm64" ;;
	esac
	# MSYS/Git-Bash on Windows is not a release target; say so rather than emit
	# a name no Release carries.
	if [ "$os" = "mingw64_nt-10.0-26200" ] || [ "${os#mingw}" != "$os" ] || [ "${os#msys}" != "$os" ]; then
		echo "rustrouter-windows-${arch}"
		return
	fi
	echo "rustrouter-${os}-${arch}"
}

# ─── self-test ────────────────────────────────────────────────────────────

if [ "${1:-}" = "--self-test" ]; then
	fail=0
	check() { # <label> <expected> <got>
		if [ "$2" != "$3" ]; then
			echo "FAIL: $1 (expected $2, got $3)"
			fail=1
		fi
	}
	# 16 GB free, 16 cpus, 1.8 GB per job, 1 GB reserve -> budget 15 GB -> 8 jobs
	check "roomy" 8 "$(compute_jobs 16384 16 1800 1024)"
	# CPU-bound: 64 cpus but only 4 GB budget -> 2 jobs
	check "cpu-bound" 2 "$(compute_jobs 5120 64 1800 1024)"
	# Tight: budget below one job -> 1
	check "tight" 1 "$(compute_jobs 2048 8 1800 1024)"
	# Negative budget (reserve exceeds free) -> 1, not a crash
	check "negative" 1 "$(compute_jobs 512 8 1800 1024)"
	if [ "$fail" -ne 0 ]; then
		echo "self-test failed"
		exit 1
	fi
	echo "self-test ok"
	exit 0
fi

# ─── args ─────────────────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
	case "$1" in
	-l | --low-mem) LOW_MEM=1 ;;
	-j | --jobs)
		shift
		JOBS="${1:-}"
		[ -n "$JOBS" ] || { echo "[ERROR] --jobs needs a number"; exit 1; }
		;;
	--target)
		shift
		TARGET="${1:-}"
		[ -n "$TARGET" ] || { echo "[ERROR] --target needs a triple"; exit 1; }
		;;
	--release-name) RELEASE_NAME=1 ;;
	--no-frontend) BUILD_FRONTEND=0 ;;
	-h | --help)
		sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		echo "[ERROR] unknown option: $1"
		exit 1
		;;
	esac
	shift
done

# ─── tool checks ──────────────────────────────────────────────────────────

need() {
	command -v "$1" >/dev/null 2>&1 || {
		echo "[ERROR] $1 not found on PATH."
		[ -n "${2:-}" ] && echo "        $2"
		exit 1
	}
}
need cargo
need node "Install Node.js 18+."
need npm "Install Node.js 18+."

# ─── memory plan ──────────────────────────────────────────────────────────

AVAIL_MB="$(mem_available_mb)"
CPUS="$(cpu_count)"

if [ -n "$JOBS" ]; then
	PLANNED_JOBS="$JOBS"
	JOBS_SOURCE="--jobs"
elif [ "$LOW_MEM" -eq 1 ]; then
	PLANNED_JOBS=1
	JOBS_SOURCE="--low-mem"
else
	PLANNED_JOBS="$(compute_jobs "$AVAIL_MB" "$CPUS" "$PER_JOB_MB" "$RESERVE_MB")"
	JOBS_SOURCE="auto"
fi

# The auto plan already drops to 1 when memory is tight. An explicit --jobs only
# caps parallelism; it does not change the build profile, so a caller who wants
# the memory profile too says --low-mem.
if [ "$PLANNED_JOBS" -le 1 ] && [ -z "$JOBS" ]; then
	LOW_MEM=1
fi

echo "============================================================"
echo " rustrouter production build"
echo "============================================================"
if [ "$AVAIL_MB" -gt 0 ]; then
	echo " memory:   ${AVAIL_MB} MiB available, ${CPUS} cpus"
	echo " jobs:     ${PLANNED_JOBS} (${JOBS_SOURCE}, ~${PER_JOB_MB} MiB/job, ${RESERVE_MB} MiB reserve)"
else
	echo " memory:   unknown, ${CPUS} cpus -> jobs ${PLANNED_JOBS}"
fi
if [ "$PLANNED_JOBS" -le 1 ] && [ "$AVAIL_MB" -gt 0 ] && [ "$AVAIL_MB" -lt $((PER_JOB_MB + RESERVE_MB)) ]; then
	echo " warning:  free memory is below one job's budget; expect a slow build."
	echo "           close other apps, or raise swap/zram if the build is killed."
fi
echo

export CARGO_BUILD_JOBS="$PLANNED_JOBS"
export MAKEFLAGS="-j${PLANNED_JOBS}"

# Low-memory profile. LTO holds every codegen unit's IR at once for the final
# merge, which is the peak; turning it off drops that. Codegen units are left
# alone: codegen-units = 1 is already the minimum-memory setting (it disables
# rustc's only internal parallelism), so widening it would raise, not lower,
# the peak. An explicit env from the caller wins.
if [ "$LOW_MEM" -eq 1 ]; then
	export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-off}"
	export CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-0}"
	echo " low-mem:  LTO=${CARGO_PROFILE_RELEASE_LTO} (codegen-units stays 1: the low-memory setting)"
	echo
fi

# ─── frontend ─────────────────────────────────────────────────────────────

if [ "$BUILD_FRONTEND" -eq 1 ]; then
	echo "[1/3] Building frontend ..."
	[ -f "$WEB/package.json" ] || { echo "[ERROR] web/package.json not found - run from the repo root."; exit 1; }
	cd "$WEB"
	if [ ! -d node_modules ]; then
		echo "      node_modules missing, running npm ci ..."
		npm ci
	fi
	# Vite also gets OOM-killed on a small machine; cap Node's heap to a slice
	# of what is free, and never below the default floor.
	if [ "$AVAIL_MB" -gt 0 ]; then
		NODE_HEAP_MB=$((AVAIL_MB / 2))
		[ "$NODE_HEAP_MB" -gt 2048 ] && NODE_HEAP_MB=2048
		[ "$NODE_HEAP_MB" -lt 512 ] && NODE_HEAP_MB=512
		export NODE_OPTIONS="${NODE_OPTIONS:-} --max-old-space-size=${NODE_HEAP_MB}"
	fi
	npm run build
	[ -f "$WEB/dist/index.html" ] || { echo "[ERROR] web/dist/index.html missing after the build."; exit 1; }
	echo "      frontend OK - web/dist is ready"
	cd "$ROOT"
else
	echo "[1/3] Skipping frontend (--no-frontend); using existing web/dist"
	[ -f "$WEB/dist/index.html" ] || { echo "[ERROR] web/dist/index.html missing; drop --no-frontend."; exit 1; }
fi

# ─── backend ──────────────────────────────────────────────────────────────

echo
echo "[2/3] Building backend (embeds web/dist) ..."
cd "$ROOT"

CARGO_ARGS=(build --release --locked -p rustrouter -j "$PLANNED_JOBS")
if [ -n "$TARGET" ]; then
	CARGO_ARGS+=(--target "$TARGET")
fi
cargo "${CARGO_ARGS[@]}"

# A cargo config may pin a target triple; resolve either layout.
if [ -n "$TARGET" ]; then
	BIN="$ROOT/target/$TARGET/release/rustrouter"
else
	BIN="$ROOT/target/release/rustrouter"
	if [ ! -f "$BIN" ]; then
		HOST="$(rustc -vV | awk '/^host:/ {print $2}')"
		BIN="$ROOT/target/$HOST/release/rustrouter"
	fi
fi
[ -f "$BIN" ] || { echo "[ERROR] release binary not found under target/."; exit 1; }

# ─── copy ─────────────────────────────────────────────────────────────────

echo
echo "[3/3] Copying the binary to rustrouter-binary/ ..."
mkdir -p "$OUT"
cp -f "$BIN" "$OUT/rustrouter"
chmod +x "$OUT/rustrouter"

if [ "$RELEASE_NAME" -eq 1 ]; then
	VERSION="$(tr -d '[:space:]' <"$ROOT/PROJECT_VERSION")"
	ASSET="$(platform_slug)-${VERSION}"
	cp -f "$BIN" "$OUT/$ASSET"
	chmod +x "$OUT/$ASSET"
	echo "      asset: $OUT/$ASSET"
fi

echo
echo "============================================================"
echo " Build complete"
echo "   binary: $OUT/rustrouter"
echo "   size:   $(wc -c <"$OUT/rustrouter" | tr -d ' ') bytes"
echo "============================================================"
