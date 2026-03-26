# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased](https://github.com/KyokoMiki/cladst/compare/v0.1.0...HEAD)

## [v0.1.0](https://github.com/KyokoMiki/cladst/commits/v0.1.0) - 2026-03-26

### Added

- **Encode**: Compress DSF or uncompressed DSDIFF into DST-compressed DSDIFF.
- **Decode**: Decompress DST-compressed DSDIFF back to DSF or uncompressed DSDIFF.
- **Test**: Validate DST frame integrity without producing output files.
- Multi-threaded parallel encoding with configurable thread count.
- In-place re-encoding with automatic temporary file handling.
- Cross-platform distribution for macOS, Linux, and Windows via cargo-dist.

**Full Changelog**: https://github.com/KyokoMiki/cladst/commits/v0.1.0
