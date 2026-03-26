//! CLI entry point: encode DSD->DST, decode DST->DSD, test DST integrity.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use same_file::is_same_file;

use cladst::decode::{self, OutputFormat};
use cladst::encode::{self, EncodeOptions};
use cladst::format::metadata::DsdMetadata;
use cladst::format::reader::{DsdFrameReader, DstFrameReader, DstToDsdAdapter};
use cladst::format::{StreamInput, open_dsd_file};

/// cladst — DST (Direct Stream Transfer) encoder/decoder for DSD audio.
///
/// Default mode encodes a DSF or DSDIFF file into DST-compressed DSDIFF.
/// Use -d to decode or -t to test a DST-compressed DSDIFF file.
#[derive(Parser)]
#[command(name = "cladst", version, about)]
struct Cli {
    /// Input file path.
    input: PathBuf,

    /// Output file path.
    output: Option<PathBuf>,

    /// Decode mode: read DST-compressed DSDIFF, write DSF (default) or uncompressed DSDIFF.
    #[arg(short, long, conflicts_with_all = ["test", "verify", "pred_order", "no_share_filters", "no_half_prob"])]
    decode: bool,

    /// Test mode: decode all frames and verify CRC, no output file.
    #[arg(short, long, conflicts_with_all = ["decode", "verify", "pred_order", "no_share_filters", "no_half_prob"])]
    test: bool,

    /// Verify encoding by decoding each frame inline and comparing.
    #[arg(short, long)]
    verify: bool,

    /// FIR prediction order (1-128).
    #[arg(long, default_value_t = 128, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=128))]
    pred_order: usize,

    /// Don't share filter coefficients across channels (use per-channel filters).
    #[arg(long)]
    no_share_filters: bool,

    /// Don't use HalfProb (p=128 for first pred_order samples).
    #[arg(long)]
    no_half_prob: bool,

    /// Number of encoding threads. 0 = use all cores (default), 1 = single-threaded.
    #[arg(short = 'j', long, default_value_t = 0, conflicts_with_all = ["decode", "test"])]
    threads: usize,

    /// Force overwriting of output files. Required when output file already exists
    /// or when re-encoding a file in-place (input = output).
    #[arg(short, long)]
    force: bool,
}

/// Resolve the output path, check overwrite permissions, and handle in-place
/// temp file logic. Returns (final_output_path, actual_write_path).
fn resolve_output(
    input: &Path,
    output: Option<PathBuf>,
    default_ext: &str,
    force: bool,
) -> Result<(PathBuf, PathBuf)> {
    let output_path = output.unwrap_or_else(|| input.with_extension(default_ext));

    if !force && output_path.exists() {
        bail!(
            "Output file {} already exists, use -f to force overwrite",
            output_path.display(),
        );
    }

    let same = output_path.exists() && is_same_file(input, &output_path).unwrap_or(false);
    let actual_output = if same {
        let mut tmp = output_path.clone().into_os_string();
        tmp.push(".tmp.cladst");
        PathBuf::from(tmp)
    } else {
        output_path.clone()
    };

    Ok((output_path, actual_output))
}

/// Rename temp file to final path on success. Remove temp file on error.
fn finalize_output(result: &Result<()>, actual_output: &Path, output_path: &Path) -> Result<()> {
    if result.is_err() {
        let _ = std::fs::remove_file(actual_output);
        return Ok(());
    }
    if actual_output != output_path {
        std::fs::rename(actual_output, output_path).with_context(|| {
            format!(
                "Failed to rename temp file {} to {}, the file is kept as {}",
                actual_output.display(),
                output_path.display(),
                actual_output.display(),
            )
        })?;
    }
    Ok(())
}

/// Print output file size.
fn print_output_size(path: &Path) -> Result<()> {
    let out_size = std::fs::metadata(path)
        .context("Failed to get output file size")?
        .len();
    println!(
        "  Output: {} bytes ({:.1} MB)",
        out_size,
        out_size as f64 / 1024.0 / 1024.0,
    );
    Ok(())
}

/// Print metadata summary.
fn print_metadata(meta: &DsdMetadata) {
    println!(
        "  Sample rate: {} Hz ({}x DSD)",
        meta.sample_rate,
        meta.sample_rate / 44100,
    );
    println!("  Channels: {}", meta.n_channels);
    println!("  Total samples: {}", meta.total_samples);
    println!(
        "  Frame size: {} bits/ch ({} bytes/ch)",
        meta.frame_bits, meta.frame_bytes_per_ch,
    );
    println!("  Frames: {}", meta.n_frames);
}

fn run_test(reader: &mut impl DstFrameReader) -> Result<()> {
    let meta = reader.metadata().clone();
    println!(
        "  Sample rate: {} Hz ({}x DSD), Channels: {}, Frames: {}",
        meta.sample_rate,
        meta.sample_rate / 44100,
        meta.n_channels,
        meta.n_frames,
    );
    let t0 = Instant::now();
    let n = meta.n_frames;
    let stats = decode::test(
        reader,
        Some(&|i, _n| {
            if (i + 1) % 100 == 0 || i == n - 1 {
                let elapsed = t0.elapsed().as_secs_f64();
                eprint!("\r  Frame {}/{}  {:.1}s", i + 1, n, elapsed);
            }
        }),
    )?;
    eprintln!();
    let elapsed = t0.elapsed().as_secs_f64();
    println!(
        "  OK. {} frames decoded, {} CRC verified in {:.1}s.",
        stats.total_frames, stats.crc_checked, elapsed,
    );
    Ok(())
}

fn run_decode(cli: &Cli, reader: &mut impl DstFrameReader) -> Result<()> {
    let meta = reader.metadata().clone();
    let (output_path, actual_output) =
        resolve_output(&cli.input, cli.output.clone(), "dsf", cli.force)?;

    println!(
        "  Sample rate: {} Hz ({}x DSD)",
        meta.sample_rate,
        meta.sample_rate / 44100,
    );
    println!("  Channels: {}", meta.n_channels);
    println!("  Frames: {}", meta.n_frames);
    println!("Decoding, writing {}...", actual_output.display());

    let t0 = Instant::now();
    let format = match actual_output.extension().and_then(|e| e.to_str()) {
        Some("dff") => OutputFormat::DsdiffUncompressed,
        _ => OutputFormat::Dsf,
    };
    let file = std::fs::File::create(&actual_output)
        .with_context(|| format!("Failed to create {}", actual_output.display()))?;

    let n = meta.n_frames;
    let result = decode::decode(
        reader,
        BufWriter::new(file),
        format,
        Some(&|i, _n| {
            if (i + 1) % 100 == 0 || i == n - 1 {
                let elapsed = t0.elapsed().as_secs_f64();
                eprint!("\r  Frame {}/{}  {:.1}s", i + 1, n, elapsed);
            }
        }),
    );
    eprintln!();

    let result_for_finalize = result
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"));
    finalize_output(&result_for_finalize, &actual_output, &output_path)?;
    let stats = result?;

    let elapsed = t0.elapsed().as_secs_f64();
    println!(
        "  Done in {:.1}s. {} frames decoded, {} CRC verified.",
        elapsed, stats.total_frames, stats.crc_checked,
    );
    print_output_size(&output_path)?;

    Ok(())
}

fn run_encode(cli: &Cli, reader: &mut impl DsdFrameReader) -> Result<()> {
    let meta = reader.metadata().clone();
    let (output_path, actual_output) =
        resolve_output(&cli.input, cli.output.clone(), "dff", cli.force)?;

    let options = EncodeOptions {
        pred_order: cli.pred_order,
        share_filters: !cli.no_share_filters,
        half_prob: !cli.no_half_prob,
        verify: cli.verify,
        threads: cli.threads,
    };

    print_metadata(&meta);

    let total_raw = meta.frame_bytes_per_ch * meta.n_channels;
    let threads_desc = if options.threads == 0 {
        "all".to_string()
    } else {
        options.threads.to_string()
    };
    println!(
        "Encoding with {} threads, pred_order={} share_filters={} half_prob={}, writing {}...",
        threads_desc,
        options.pred_order,
        options.share_filters,
        options.half_prob,
        actual_output.display(),
    );

    let t0 = Instant::now();
    let verify = cli.verify;
    let n_frames = meta.n_frames;
    let file = std::fs::File::create(&actual_output)
        .with_context(|| format!("Failed to create {}", actual_output.display()))?;

    let result = encode::encode(
        reader,
        BufWriter::new(file),
        &options,
        Some(&|p| {
            if (p.frame + 1) % 100 == 0 || p.frame == n_frames - 1 {
                let elapsed = t0.elapsed().as_secs_f64();
                let ratio = if total_raw > 0 {
                    p.compressed_bytes as f64 / ((p.frame + 1) as f64 * total_raw as f64)
                } else {
                    0.0
                };
                let verify_tag = if verify { " V" } else { "" };
                eprint!(
                    "\r  Frame {}/{}  DST:{} RAW:{}  ratio:{:.3}  {:.1}s{}",
                    p.frame + 1,
                    p.total_frames,
                    p.dst_frames,
                    p.raw_frames,
                    ratio,
                    elapsed,
                    verify_tag,
                );
            }
        }),
    );
    eprintln!();

    let result_for_finalize = result
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"));
    finalize_output(&result_for_finalize, &actual_output, &output_path)?;
    let stats = result?;

    let elapsed = t0.elapsed().as_secs_f64();
    println!(
        "  Done in {:.1}s. DST:{} RAW:{} ratio:{:.3}",
        elapsed, stats.dst_frames, stats.raw_frames, stats.compression_ratio,
    );
    print_output_size(&output_path)?;

    if verify {
        println!("Verify: all frames decoded correctly.");
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("Opening {}...", cli.input.display());
    let file = File::open(&cli.input)
        .with_context(|| format!("Failed to open {}", cli.input.display()))?;
    let input = open_dsd_file(file)?;

    match input {
        StreamInput::Dst(mut dst_reader) => {
            if cli.test {
                if cli.output.is_some() {
                    bail!("Output file is not used in test mode");
                }
                return run_test(&mut dst_reader);
            }
            if cli.decode {
                return run_decode(&cli, &mut dst_reader);
            }
            // Encode mode: DST input -> decode on-the-fly -> re-encode
            println!("  DST input detected, re-encoding...");
            let mut adapter = DstToDsdAdapter::new(dst_reader);
            run_encode(&cli, &mut adapter)
        }
        StreamInput::Dsd(mut dsd_reader) => {
            if cli.test {
                bail!("Test mode requires a DST-compressed DSDIFF file");
            }
            if cli.decode {
                bail!("Decode mode requires a DST-compressed DSDIFF file");
            }
            run_encode(&cli, &mut dsd_reader)
        }
    }
}
