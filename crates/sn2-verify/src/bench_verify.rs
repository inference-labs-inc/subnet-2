use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "bench-verify", about = "Benchmark verification concurrency")]
struct Args {
    #[arg(long, help = "Path to circuit bundle directory")]
    circuit_path: PathBuf,

    #[arg(long, help = "Path to .onnx file for slice proof generation")]
    onnx_path: Option<PathBuf>,

    #[arg(
        long,
        default_value = "48",
        help = "Total verifications per concurrency level"
    )]
    iterations: usize,

    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1,2,4,8,12,16,20,24,32,48,64",
        help = "Concurrency levels to test"
    )]
    levels: Vec<usize>,
}

fn generate_proof(circuit_path: &std::path::Path) -> Result<(Vec<u8>, Vec<u8>, usize)> {
    let backend = dsperse::backend::jstprove::JstproveBackend::new();

    let params = backend
        .load_params(circuit_path)
        .map_err(|e| anyhow::anyhow!("loading circuit params: {e}"))?;

    let num_inputs = params
        .as_ref()
        .map(|p| p.effective_input_dims())
        .unwrap_or(0);

    let dummy_input: Vec<f64> = vec![0.5; num_inputs.max(1)];

    let witness_bytes = backend
        .witness_f64(circuit_path, &dummy_input, &[])
        .map_err(|e| anyhow::anyhow!("witness generation: {e}"))?;

    let proof_bytes = backend
        .prove(circuit_path, &witness_bytes)
        .map_err(|e| anyhow::anyhow!("proof generation: {e}"))?;

    eprintln!(
        "generated proof: witness={}B proof={}B num_inputs={}",
        witness_bytes.len(),
        proof_bytes.len(),
        num_inputs
    );

    Ok((witness_bytes, proof_bytes, num_inputs))
}

async fn run_bench(
    circuit_path: &str,
    witness_hex: &str,
    proof_hex: &str,
    num_inputs: usize,
    concurrency: usize,
    iterations: usize,
) -> Duration {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let circuit_path = Arc::new(circuit_path.to_string());
    let witness_hex = Arc::new(witness_hex.to_string());
    let proof_hex = Arc::new(proof_hex.to_string());

    let start = Instant::now();
    let mut handles = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let cp = Arc::clone(&circuit_path);
        let wh = Arc::clone(&witness_hex);
        let ph = Arc::clone(&proof_hex);
        let req_id = format!("bench-{i}");

        handles.push(tokio::spawn(async move {
            let result = sn2_verify::verify::verify_inner(
                &req_id,
                &cp,
                &wh,
                &ph,
                num_inputs,
                &None,
                "raw",
            )
            .await;
            drop(permit);
            result.is_ok()
        }));
    }

    let mut ok = 0usize;
    let mut fail = 0usize;
    for h in handles {
        if h.await.unwrap_or(false) {
            ok += 1;
        } else {
            fail += 1;
        }
    }

    let elapsed = start.elapsed();

    if fail > 0 {
        eprintln!("  WARNING: {fail}/{iterations} verifications failed");
    }
    let _ = ok;

    elapsed
}

#[tokio::main]
async fn main() -> Result<()> {
    sn2_types::init_tracing("warn");

    let args = Args::parse();
    let circuit_path_str = args.circuit_path.to_string_lossy().to_string();

    eprintln!("generating proof from circuit: {circuit_path_str}");
    let (witness_bytes, proof_bytes, num_inputs) =
        tokio::task::spawn_blocking({
            let cp = args.circuit_path.clone();
            move || generate_proof(&cp)
        })
        .await
        .context("proof generation task panicked")??;

    let witness_hex = hex::encode(&witness_bytes);
    let proof_hex = hex::encode(&proof_bytes);

    eprintln!("warming circuit cache...");
    sn2_verify::verify::verify_inner(
        "warmup",
        &circuit_path_str,
        &witness_hex,
        &proof_hex,
        num_inputs,
        &None,
        "raw",
    )
    .await
    .context("warmup verification failed")?;
    eprintln!("cache warm");

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    eprintln!(
        "available_parallelism={cpus}, iterations={}, levels={:?}",
        args.iterations, args.levels
    );
    eprintln!();
    eprintln!("{:<12} {:>12} {:>12} {:>12}", "concurrency", "elapsed_s", "verify/s", "ms/verify");
    eprintln!("{}", "-".repeat(52));

    for &level in &args.levels {
        let elapsed = run_bench(
            &circuit_path_str,
            &witness_hex,
            &proof_hex,
            num_inputs,
            level,
            args.iterations,
        )
        .await;

        let secs = elapsed.as_secs_f64();
        let throughput = args.iterations as f64 / secs;
        let ms_per = secs * 1000.0 / args.iterations as f64;

        eprintln!(
            "{:<12} {:>12.2} {:>12.2} {:>12.1}",
            level, secs, throughput, ms_per
        );
    }

    Ok(())
}
