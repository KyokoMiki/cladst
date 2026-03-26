# cladst

[![Crates.io](https://img.shields.io/crates/v/cladst)](https://crates.io/crates/cladst)
[![License: GPL-3.0](https://img.shields.io/crates/l/cladst)](https://github.com/KyokoMiki/cladst/blob/main/LICENSE)

DST (Direct Stream Transfer) encoder/decoder for DSD audio, written in Rust.

Encode DSF or DSDIFF files into DST-compressed DSDIFF, decode DST-compressed DSDIFF back to DSF or uncompressed DSDIFF, or verify DST frame integrity.

## Installation

### From crates.io

```sh
cargo install cladst
```

### From source

```sh
git clone https://github.com/KyokoMiki/cladst.git
cd cladst
cargo install --path .
```

## Usage

### Encode (default)

Encode a DSF or DSDIFF file into DST-compressed DSDIFF:

```sh
# Auto-detect input format, output to .dff
cladst input.dsf

# Specify output path
cladst input.dsf output.dff

# Verify encoding by decoding each frame inline (like FLAC --verify)
cladst input.dsf -v
```

### Decode

Decode a DST-compressed DSDIFF file:

```sh
# Decode to DSF (default)
cladst -d input.dff

# Decode to uncompressed DSDIFF
cladst -d input.dff output.dff

# Specify output path
cladst -d input.dff output.dsf
```

### Test

Verify DST frame integrity without producing output:

```sh
cladst -t input.dff
```

### Options

| Option | Description |
| --- | --- |
| `-d, --decode` | Decode mode: DST-compressed DSDIFF → DSF or uncompressed DSDIFF |
| `-t, --test` | Test mode: decode all frames and verify CRC, no output |
| `-v, --verify` | Verify encoding by decoding each frame inline |
| `-f, --force` | Force overwrite existing output files |
| `--pred-order <N>` | FIR prediction order, 1–128 (default: 128) |
| `--no-share-filters` | Use per-channel filters instead of shared |
| `--no-half-prob` | Disable HalfProb (p=128 for first pred_order samples) |

## Supported Formats

| Format | Read | Write |
| --- | --- | --- |
| DSF (.dsf) | ✅ | ✅ (decode output) |
| DSDIFF (.dff) uncompressed | ✅ | ✅ (decode output) |
| DSDIFF (.dff) DST-compressed | ✅ | ✅ (encode output) |

## Building

Requires [Rust](https://www.rust-lang.org/tools/install) (stable toolchain, edition 2024).

```sh
cargo build --release
```

## License

[GPL-3.0](LICENSE)
