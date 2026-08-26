# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/metzenseifner/sismatic/compare/v0.3.1...v0.4.0) - 2026-08-26

### Added

- *(core)* add rtmp stream support write side
- *(core)* add streams 1 2 3 queries

## [0.3.0](https://github.com/metzenseifner/sismatic/compare/v0.2.25...v0.3.0) - 2026-08-22

### Added

- [**breaking**] add group endpoints to readings

## [0.2.25](https://github.com/metzenseifner/sismatic/compare/v0.2.24...v0.2.25) - 2026-08-14

### Other

- build: publish linux-aarch64 wheels
- release tooling: internal path dependencies carry a version requirement again
  (`version = "0"`), without which release-plz cannot package the workspace. Cut
  by hand to move the release baseline off the v0.2.24 tag, whose tree cannot be
  packaged; no library changes since 0.2.24.

## [0.2.24](https://github.com/metzenseifner/sismatic/compare/v0.2.23...v0.2.24) - 2026-08-10

### Added

- add new GET HTTP routes to read fields from the store

## [0.2.20](https://github.com/metzenseifner/sismatic/compare/v0.2.19...v0.2.20) - 2026-07-27

### Other

- update Cargo.lock dependencies
