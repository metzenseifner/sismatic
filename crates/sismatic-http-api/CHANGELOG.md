# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/metzenseifner/sismatic/compare/v0.4.0...v0.4.1) - 2026-09-05

### Added

- *(http-api)* add exploratory endpoints

### Other

- rename readings>reads, writings>writes
- rename "phase" to "desired_recording_state"
- command disambiguity by renaming category of commands to writings corresponding to readings.

## [0.4.0](https://github.com/metzenseifner/sismatic/compare/v0.3.1...v0.4.0) - 2026-08-26

### Added

- routes are now scoped by domain ([#40](https://github.com/metzenseifner/sismatic/pull/40))

### Fixed

- *(http-api)* drift from commands to actual paths served

## [0.3.1](https://github.com/metzenseifner/sismatic/compare/v0.3.0...v0.3.1) - 2026-08-25

### Added

- openapi ui switch to scalar ui from swagger ui

## [0.3.0](https://github.com/metzenseifner/sismatic/compare/v0.2.25...v0.3.0) - 2026-08-22

### Added

- [**breaking**] add group endpoints to readings
- status codes improved for consumer error handling
- give ApiError a rejection reason distinguisher for consumer
- add device catalog support
- add write support to sismatic-store
