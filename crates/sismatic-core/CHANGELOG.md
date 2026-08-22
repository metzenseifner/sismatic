# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/metzenseifner/sismatic/compare/v0.2.25...v0.3.0) - 2026-08-22

### Added

- [**breaking**] add group endpoints to readings
- status codes improved for consumer error handling
- give ApiError a rejection reason distinguisher for consumer
- add write support to sismatic-store

## [0.2.25](https://github.com/metzenseifner/sismatic/compare/v0.2.24...v0.2.25) - 2026-08-14

### Other

- release tooling: internal path dependencies carry a version requirement again
  (`version = "0"`), without which release-plz cannot package the workspace. Cut
  by hand to move the release baseline off the v0.2.24 tag, whose tree cannot be
  packaged; no library changes since 0.2.24.

## [0.2.19](https://github.com/metzenseifner/sismatic/releases/tag/v0.2.19) - 2026-07-21

### Added

- device groups: one id can address a set of devices; a group call fans out
  concurrently and reports per-member results (CLI, Python, and web facades)
- eager retries: `eager_retry_secs` keeps a warm connection by re-attempting
  cold devices on a short cadence instead of giving up after the first failure
- log info on connection established

## [0.2.18](https://github.com/metzenseifner/sismatic/releases/tag/v0.2.18) - 2026-07-18

### Added

- add secrecy to protect leakage of passwords in config.
- make connect_secs, command_secs, port optional with hardcoded defaults
- eager connect for context to devices on startup and sis keepalive configurable
- *(sismatic-core)* added telemetry data for observability when debugging communication
- fix: auth now supports keyboard-interactive as fallback since smps do not support passwordauth. Adds python logging
- split into multiple crates to develop more functionality around shared core

### Fixed

- out-of-range port like 99999 edge case to avoid silent truncation
- register write response parsing and adds telemetry of instruction contruction
- register queries and adds missing queries
- queries parsers should not work properly
- smp banner drainage and unit name parsing
- read drives channel.wait() and accepts both ChannelMsg::Data (stdout) and ChannelMsg::ExtendedData (stderr), so reply sometimes on stderr reaches the parser

### Other

- Config sources ([#4](https://github.com/metzenseifner/sismatic/pull/4))
- format-agnostic config support to core with opinions wrapped as features for convenience.
- rename keepalive to sis_keepalive to avoid confusion with ssh keepalive
- integration tests of RusshConnector now support response to query unit_name
- add ssh server to simulate real extron smp device in integration tests

## [0.2.17](https://github.com/metzenseifner/sismatic/releases/tag/v0.2.17) - 2026-07-17

### Added

- eager connect for context to devices on startup and sis keepalive configurable
- *(sismatic-core)* added telemetry data for observability when debugging communication
- fix: auth now supports keyboard-interactive as fallback since smps do not support passwordauth. Adds python logging
- split into multiple crates to develop more functionality around shared core

### Fixed

- out-of-range port like 99999 edge case to avoid silent truncation
- register write response parsing and adds telemetry of instruction contruction
- register queries and adds missing queries
- queries parsers should not work properly
- smp banner drainage and unit name parsing
- read drives channel.wait() and accepts both ChannelMsg::Data (stdout) and ChannelMsg::ExtendedData (stderr), so reply sometimes on stderr reaches the parser

### Other

- format-agnostic config support to core with opinions wrapped as features for convenience.
- rename keepalive to sis_keepalive to avoid confusion with ssh keepalive
- integration tests of RusshConnector now support response to query unit_name
- add ssh server to simulate real extron smp device in integration tests
