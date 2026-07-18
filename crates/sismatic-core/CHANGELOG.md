# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
