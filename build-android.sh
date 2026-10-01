#!/usr/bin/env bash
#
# rustrouter production build for Android aarch64 (Termux).
#
# Two modes, chosen automatically:
#
#   native  - run inside Termux on an aarch64 device. rustc, clang and the
#             bionic sysroot are already there (`pkg install rust clang
#             ndk-sysroot binutils`), so the android target is the host's own
#             architecture and nothing is cross-compiled.
#   cross   - run on a desktop Linux/macOS host. Needs the Android NDK; the
#             script finds it or takes ANDROID_NDK_HOME.
#
# TLS note: the rustls backend is aws-lc-sys. For `aarch64-linux-android` it
# uses its pre-generated bindings and the `cc` builder (builder/main.rs), so it
# needs a C compiler only - no cmake, no bindgen, no Go. That is what makes a
# native Termux build possible.
#
# Memory: the workspace release profile uses codegen-units = 1 and thin LTO.
# Codegen is rustc's only default parallelism, so codegen-units = 1 already
# keeps one rustc small; the peak comes from how many crates build at once and
# from the LTO merge. A phone will OOM-kill a default build. This script sizes
# the job count to the memory actually free and drops LTO when memory is tight.
# The bundled C in aws-lc-sys, libsqlite3-sys and zstd-sys compiles one file at
# a time (cc-rs has no `parallel` feature enabled), so it adds little.
#
# Output: rustrouter-binary/rustrouter-linux-android-aarch64[-<version>]
#
#   ./build-android.sh                 auto-size jobs to available RAM
#   ./build-android.sh --low-mem       force the low-memory profile and -j1
#   ./build-android.sh --jobs 2        cap parallelism explicitly
#   ./build-android.sh --release-name  also emit the CI asset name with version
#   ./build-android.sh --no-frontend   reuse an existing web/dist (backend only)
#   ./build-android.sh --self-test     run the sizing self-check and exit
#
# Knobs: RUSTROUTER_MEM_PER_JOB_MB (default 1800), RUSTROUTER_MEM_RESERVE_MB
# (default 1024), ANDROID_API_LEVEL (default 24), ANDROID_NDK_HOME.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB="$ROOT/web"
OUT="$ROOT/rustrouter-binary"
TARGET_TRIPLE="aarch64-linux-android"

PER_JOB_MB="${RUSTROUTER_MEM_PER_JOB_MB:-1800}"
RESERVE_MB="${RUSTROUTER_MEM_RESERVE_MB:-1024}"
API_LEVEL="${ANDROID_API_LEVEL:-24}"

LOW_MEM=0
JOBS=""
BUILD_FRONTEND=1
RELEASE_NAME=0
SELF_TEST=0

# ─── memory sizing (pure, so it can be self-tested) ───────────────────────

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

# Termux sets `uname -o` to Android; a plain Linux host reports GNU/Linux.
is_termux() {
	case "$(uname -o 2>/dev/null || true)" in
	Android) return 0 ;;
	esac
	case "${PREFIX:-}" in
	*com.termux*) return 0 ;;
	esac
	[ -d /data/data/com.termux ]
}

# The NDK toolchain dir for this host, or empty.
find_ndk() {
	local candidates=()
	if [ -n "${ANDROID_NDK_HOME:-}" ]; then candidates+=("$ANDROID_NDK_HOME"); fi
	if [ -n "${ANDROID_NDK_ROOT:-}" ]; then candidates+=("$ANDROID_NDK_ROOT"); fi
	if [ -n "${ANDROID_HOME:-}" ]; then candidates+=("$ANDROID_HOME/ndk"); fi
	if [ -n "${ANDROID_SDK_ROOT:-}" ]; then candidates+=("$ANDROID_SDK_ROOT/ndk"); fi
	candidates+=("$HOME/Android/Sdk/ndk" "/opt/android-ndk" "/usr/local/lib/android/sdk/ndk")

	local cand latest=""
	for cand in "${candidates[@]}"; do
		[ -d "$cand" ] || continue
		if [ -d "$cand/toolchains/llvm/prebuilt" ]; then
			echo "$cand" # ANDROID_NDK_HOME points at the NDK itself
			return
		fi
		# An SDK `ndk/` dir holds one versioned subdir; take the last.
		for sub in "$cand"/*/; do
			[ -d "$sub/toolchains/llvm/prebuilt" ] && latest="${sub%/}"
		done
	done
	if [ -n "$latest" ]; then echo "$latest"; fi
}

ndk_host_tag() {
	case "$(uname -s)" in
	Linux) echo "linux-x86_64" ;;
	Darwin) echo "darwin-x86_64" ;;
	*) echo "linux-x86_64" ;;
	esac
}

# ─── self-test ────────────────────────────────────────────────────────────

if [ "${1:-}" = "--self-test" ]; then
	fail=0
	check() {
		if [ "$2" != "$3" ]; then
			echo "FAIL: $1 (expected $2, got $3)"
			fail=1
		fi
	}
	check "roomy" 8 "$(compute_jobs 16384 16 1800 1024)"
	check "cpu-bound" 2 "$(compute_jobs 5120 64 1800 1024)"
	check "tight" 1 "$(compute_jobs 2048 8 1800 1024)"
	check "negative" 1 "$(compute_jobs 512 8 1800 1024)"
	# A phone with 6 GB total, ~2.5 GB free, 8 cores -> 1 job.
	check "phone" 1 "$(compute_jobs 2560 8 1800 1024)"
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
	--release-name) RELEASE_NAME=1 ;;
	--no-frontend) BUILD_FRONTEND=0 ;;
	-h | --help)
		sed -n '2,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		echo "[ERROR] unknown option: $1"
		exit 1
		;;
	esac
	shift
done

need() {
	command -v "$1" >/dev/null 2>&1 || {
		echo "[ERROR] $1 not found on PATH."
		[ -n "${2:-}" ] && echo "        $2"
		exit 1
	}
}
need cargo
need node "Install Node.js 18+ (pkg install nodejs-lts)."
need npm "Install Node.js 18+ (pkg install nodejs-lts)."

echo "============================================================"
echo " rustrouter production build - Android aarch64 (Termux)"
echo "============================================================"

# ─── toolchain selection ──────────────────────────────────────────────────

if is_termux; then
	echo " mode:     native Termux"
	# The host is already aarch64 bionic, so clang from Termux's toolchain is
	# the right compiler; ndk-sysroot supplies the headers and stub libs.
	need clang "pkg install clang"
	CC_BIN="${CC:-clang}"
	AR_BIN="${AR:-llvm-ar}"
	if ! command -v "$AR_BIN" >/dev/null 2>&1; then
		AR_BIN="$(command -v ar || true)"
	fi
	if [ ! -d "${PREFIX:-}/include" ] && [ ! -d /data/data/com.termux/files/usr/include ]; then
		echo " warning:  ndk-sysroot headers not found; if the C build fails, run:"
		echo "             pkg install ndk-sysroot binutils"
	fi
else
	echo " mode:     cross (host $(uname -s) $(uname -m))"
	NDK="$(find_ndk)"
	if [ -z "$NDK" ]; then
		echo "[ERROR] Android NDK not found."
		echo "        Set ANDROID_NDK_HOME to the NDK path, or install one, e.g."
		echo "          sdkmanager 'ndk;27.2.12479018'"
		exit 1
	fi
	BIN_DIR="$NDK/toolchains/llvm/prebuilt/$(ndk_host_tag)/bin"
	[ -d "$BIN_DIR" ] || { echo "[ERROR] NDK toolchain missing: $BIN_DIR"; exit 1; }
	CC_BIN="$BIN_DIR/aarch64-linux-android${API_LEVEL}-clang"
	AR_BIN="$BIN_DIR/llvm-ar"
	[ -x "$CC_BIN" ] || { echo "[ERROR] compiler not found: $CC_BIN"; exit 1; }
	echo " ndk:      $NDK (API $API_LEVEL)"
fi

# Ensure the Rust target is installed when rustup is available. `pkg install
# rust` may already ship it, so a rustup failure is a warning, not a stop.
if command -v rustup >/dev/null 2>&1; then
	if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET_TRIPLE"; then
		echo "           installing rust target $TARGET_TRIPLE ..."
		rustup target add "$TARGET_TRIPLE" || echo " warning:  rustup target add failed; continuing."
	fi
fi

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
if [ "$PLANNED_JOBS" -le 1 ] && [ -z "$JOBS" ]; then
	LOW_MEM=1
fi

if [ "$AVAIL_MB" -gt 0 ]; then
	echo " memory:   ${AVAIL_MB} MiB available, ${CPUS} cpus"
	echo " jobs:     ${PLANNED_JOBS} (${JOBS_SOURCE}, ~${PER_JOB_MB} MiB/job, ${RESERVE_MB} MiB reserve)"
else
	echo " memory:   unknown, ${CPUS} cpus -> jobs ${PLANNED_JOBS}"
fi
if [ "$AVAIL_MB" -gt 0 ] && [ "$AVAIL_MB" -lt $((PER_JOB_MB + RESERVE_MB)) ]; then
	echo " warning:  free memory is below one job's budget; expect a slow build."
	echo "           close other apps, or add zram/swap if the build is killed."
fi
echo

export CARGO_BUILD_JOBS="$PLANNED_JOBS"
export MAKEFLAGS="-j${PLANNED_JOBS}"

# The C deps compile through `cc`, which reads the per-target CC/AR. Set both
# the prefixed and the target-triple env so every crate picks them up.
export "CC_${TARGET_TRIPLE}=$CC_BIN"
export "AR_${TARGET_TRIPLE}=$AR_BIN"
export CC="$CC_BIN"
export AR="$AR_BIN"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_BIN"

# Cross builds need the target to be explicit; a native Termux build does too,
# because the host triple (aarch64-linux-android or a GNU one) may differ from
# the artifact we want.
export CARGO_BUILD_TARGET="$TARGET_TRIPLE"

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
	# Vite is memory-hungry; cap Node's heap to half of free, clamped.
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
cargo build --release --locked -p rustrouter --target "$TARGET_TRIPLE" -j "$PLANNED_JOBS"

BIN="$ROOT/target/$TARGET_TRIPLE/release/rustrouter"
[ -f "$BIN" ] || { echo "[ERROR] release binary not found: $BIN"; exit 1; }

# ─── copy ─────────────────────────────────────────────────────────────────

echo
echo "[3/3] Copying the binary to rustrouter-binary/ ..."
mkdir -p "$OUT"
ASSET="rustrouter-linux-android-aarch64"
cp -f "$BIN" "$OUT/$ASSET"
chmod +x "$OUT/$ASSET"

if [ "$RELEASE_NAME" -eq 1 ]; then
	VERSION="$(tr -d '[:space:]' <"$ROOT/PROJECT_VERSION")"
	cp -f "$BIN" "$OUT/${ASSET}-${VERSION}"
	chmod +x "$OUT/${ASSET}-${VERSION}"
	echo "      asset: $OUT/${ASSET}-${VERSION}"
fi

echo
echo "============================================================"
echo " Build complete"
echo "   binary: $OUT/$ASSET"
echo "   size:   $(wc -c <"$OUT/$ASSET" | tr -d ' ') bytes"
echo "   target: $TARGET_TRIPLE"
echo "============================================================"
