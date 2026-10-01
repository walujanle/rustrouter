# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is semantic.

## [Unreleased]

## [0.1.1] - 2026-10-01

### Fixed

- `get_executor` no longer races on a cold provider key. It did a `get` then an
  `insert` on the `DashMap`, so two threads could both miss, both build a
  `DefaultExecutor`, and hand back different instances; the
  "cached per provider" test failed intermittently. A `entry().or_insert_with`
  holds the shard lock across the check and the insert.
- The Android reclaim trim resolves `mallopt` with `dlsym` instead of calling it
  directly. `mallopt` is `__INTRODUCED_IN(26)` and `cargo-ndk` links against API
  21, so the direct call failed to link with an undefined symbol. The constant
  was wrong too: bionic's `M_PURGE` is `-101`, not `-6`. Where the platform
  predates the symbol the trim is a no-op.
- The Android aarch64 release leg runs as its own job on `ubuntu-24.04` instead
  of a matrix entry on `ubuntu-24.04-arm`. The ARM runner image ships no Android
  SDK, and Google publishes the NDK only for an x86_64 Linux host, so
  `cargo ndk` failed with "Could not find any NDK". The x64 image presets
  `ANDROID_NDK_HOME`. The native legs no longer carry the android-only steps.

## [0.1.0] - 2026-10-01

Initial release.

[Unreleased]: https://github.com/walujanle/rustrouter/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.1
[0.1.0]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.0
