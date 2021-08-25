# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

# Unreleased

### Added

### Removed

### Changed

### Fixed

# [0.10.9] - 2021-08-25

### Changed

- Switched from `spinning_top` to `spin`

# [0.10.8] - 2021-08-06

### Changed

- Updated `nanorand` to `0.6`

# [0.10.7] - 2021-06-10

### Fixed

- Removed accidental nightly-only syntax

# [0.10.6] - 2021-06-10

### Added

- `fn into_inner(self) -> T` for send errors, allowing for easy access to the unsent message

# [0.10.5] - 2021-04-26

### Added

- `is_disconnected`, `is_empty`, `is_full`, `len`, and `capacity` on future types

# [0.10.4] - 2021-04-12

### Fixed

- Shutdown-related race condition with async recv that caused spurious errors

# [0.10.3] - 2021-04-09

### Fixed

- Compilation error when enabling `select` without `eventual_fairness`

# [0.10.2] - 2021-02-07

### Fixed

- Incorrect pointer comparison in `Selector` causing missing receives

# [0.10.1] - 2020-12-30

### Removed

- Removed `T: Unpin` requirement from async traits using `pin_project`

# [0.10.0] - 2020-12-09

### Changed

- Renamed `SendFuture` to `SendFut` to be consistent with `RecvFut`
- Improved async-related documentation

### Fixed

- Updated `nanorand` to address security advisory
