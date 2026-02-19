use anyhow::{bail, Context, Result};
use num_bigint::{BigInt, BigUint};
use std::io::{Cursor, Read};
use std::path::Path;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const MIN_PUBLIC_INPUTS: usize = 2;

#[allow(dead_code)]
pub struct WitnessData {
    pub num_witnesses: u64,
    pub num_inputs_per_witness: u64,
    pub num_public_inputs_per_witness: u64,
    pub modulus: BigUint,
    pub witnesses: Vec<Witness>,
}

#[allow(dead_code)]
pub struct Witness {
    pub inputs: Vec<BigUint>,
    pub public_inputs: Vec<BigUint>,
}

#[allow(dead_code)]
pub struct ExtractedIO {
    pub inputs: Vec<BigUint>,
    pub raw_outputs: Vec<BigUint>,
    pub signed_outputs: Vec<BigInt>,
    pub rescaled_outputs: Vec<f64>,
    pub scale_base: u64,
    pub scale_exponent: u64,
    pub modulus: BigUint,
}

pub fn decompress_if_needed(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 4 && data[..4] == ZSTD_MAGIC {
        zstd::bulk::decompress(data, 512 * 1024 * 1024).context("zstd decompression failed")
    } else {
        Ok(data.to_vec())
    }
}

fn read_u64_le(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_u256_le(cursor: &mut Cursor<&[u8]>) -> Result<BigUint> {
    let mut buf = [0u8; 32];
    cursor.read_exact(&mut buf)?;
    Ok(BigUint::from_bytes_le(&buf))
}

pub fn load_witness_from_bytes(raw: &[u8]) -> Result<WitnessData> {
    let data = decompress_if_needed(raw)?;
    let mut cursor = Cursor::new(data.as_slice());

    let num_witnesses = read_u64_le(&mut cursor)?;
    let num_inputs = read_u64_le(&mut cursor)?;
    let num_public_inputs = read_u64_le(&mut cursor)?;
    let modulus = read_u256_le(&mut cursor)?;

    let per_witness = (num_inputs as usize)
        .checked_add(num_public_inputs as usize)
        .context("witness header overflow: num_inputs + num_public_inputs")?;
    let total_elements = (num_witnesses as usize)
        .checked_mul(per_witness)
        .context("witness header overflow: num_witnesses * per_witness")?;
    let total_bytes = total_elements
        .checked_mul(32)
        .context("witness header overflow: total_elements * 32")?;
    let mut bulk = vec![0u8; total_bytes];
    cursor
        .read_exact(&mut bulk)
        .context("witness data truncated")?;

    let mut witnesses = Vec::with_capacity(num_witnesses as usize);

    for w in 0..num_witnesses as usize {
        let base = w * per_witness;
        let mut inputs = Vec::with_capacity(num_inputs as usize);
        for i in 0..num_inputs as usize {
            let offset = (base + i) * 32;
            inputs.push(BigUint::from_bytes_le(&bulk[offset..offset + 32]));
        }
        let mut public_inputs = Vec::with_capacity(num_public_inputs as usize);
        for i in 0..num_public_inputs as usize {
            let offset = (base + num_inputs as usize + i) * 32;
            public_inputs.push(BigUint::from_bytes_le(&bulk[offset..offset + 32]));
        }
        witnesses.push(Witness {
            inputs,
            public_inputs,
        });
    }

    Ok(WitnessData {
        num_witnesses,
        num_inputs_per_witness: num_inputs,
        num_public_inputs_per_witness: num_public_inputs,
        modulus,
        witnesses,
    })
}

#[allow(dead_code)]
pub fn load_witness_from_file(path: &Path) -> Result<WitnessData> {
    let raw =
        std::fs::read(path).with_context(|| format!("reading witness file: {}", path.display()))?;
    load_witness_from_bytes(&raw)
}

pub fn extract_io(witness_data: &WitnessData, num_inputs: usize) -> Result<ExtractedIO> {
    if witness_data.witnesses.is_empty() {
        bail!("no witnesses in witness data");
    }

    let public_inputs = &witness_data.witnesses[0].public_inputs;
    if public_inputs.len() < MIN_PUBLIC_INPUTS {
        bail!(
            "public_inputs too short: {} < {}",
            public_inputs.len(),
            MIN_PUBLIC_INPUTS
        );
    }

    if num_inputs > public_inputs.len() - MIN_PUBLIC_INPUTS {
        bail!(
            "num_inputs {} exceeds available public_inputs (len={}, min_reserved={})",
            num_inputs,
            public_inputs.len(),
            MIN_PUBLIC_INPUTS
        );
    }

    let modulus = &witness_data.modulus;
    let inputs = public_inputs[..num_inputs].to_vec();
    let raw_outputs = public_inputs[num_inputs..public_inputs.len() - 2].to_vec();

    let scale_base_idx = public_inputs.len() - 2;
    let scale_exp_idx = public_inputs.len() - 1;

    use num_traits::ToPrimitive;
    let scale_base = public_inputs[scale_base_idx]
        .to_u64()
        .context("scale_base does not fit in u64")?;
    let scale_exponent = public_inputs[scale_exp_idx]
        .to_u64()
        .context("scale_exponent does not fit in u64")?;

    let signed_outputs: Vec<BigInt> = raw_outputs
        .iter()
        .map(|v| crate::field::from_field_repr(v, modulus))
        .collect();

    let rescaled_outputs =
        crate::field::descale_outputs(&signed_outputs, scale_base, scale_exponent);

    Ok(ExtractedIO {
        inputs,
        raw_outputs,
        signed_outputs,
        rescaled_outputs,
        scale_base,
        scale_exponent,
        modulus: modulus.clone(),
    })
}
