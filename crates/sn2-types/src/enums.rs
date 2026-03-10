use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum ProofSystem {
    CIRCOM,
    JSTPROVE,
}

impl std::fmt::Display for ProofSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CIRCOM => write!(f, "CIRCOM"),
            Self::JSTPROVE => write!(f, "JSTPROVE"),
        }
    }
}

impl FromStr for ProofSystem {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "CIRCOM" => Ok(Self::CIRCOM),
            "JSTPROVE" => Ok(Self::JSTPROVE),
            other => Err(format!("unknown proof system: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum CircuitType {
    PROOF_OF_WEIGHTS,
    PROOF_OF_COMPUTATION,
    DSPERSE_PROOF_GENERATION,
}

impl std::fmt::Display for CircuitType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PROOF_OF_WEIGHTS => write!(f, "PROOF_OF_WEIGHTS"),
            Self::PROOF_OF_COMPUTATION => write!(f, "PROOF_OF_COMPUTATION"),
            Self::DSPERSE_PROOF_GENERATION => write!(f, "DSPERSE_PROOF_GENERATION"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestType {
    #[serde(rename = "benchmark_request")]
    Benchmark,
    #[serde(rename = "real_world_request")]
    Rwr,
    #[serde(rename = "dslice_request")]
    DSlice,
    #[serde(rename = "proof_of_weights")]
    ProofOfWeights,
}

impl std::fmt::Display for RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Benchmark => write!(f, "benchmark_request"),
            Self::Rwr => write!(f, "real_world_request"),
            Self::DSlice => write!(f, "dslice_request"),
            Self::ProofOfWeights => write!(f, "proof_of_weights"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    Benchmark,
    Api,
}
