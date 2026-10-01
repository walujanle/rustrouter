# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is semantic.

## [Unreleased]

### Fixed

- The Android aarch64 release leg runs as its own job on `ubuntu-24.04` instead
  of a matrix entry on `ubuntu-24.04-arm`. The ARM runner image ships no Android
  SDK, and Google publishes the NDK only for an x86_64 Linux host, so
  `cargo ndk` failed with "Could not find any NDK". The x64 image presets
  `ANDROID_NDK_HOME`. The native legs no longer carry the android-only steps.

## [0.1.0] - 2026-10-01

Initial release.

[Unreleased]: https://github.com/walujanle/rustrouter/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.0
