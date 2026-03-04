use std::str::FromStr;

use serde::de::{self, Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[allow(non_camel_case_types)]
pub enum ProofSystem {
    ZKML,
    CIRCOM,
    JOLT,
    EZKL,
    JSTPROVE,
}

const PROOF_SYSTEM_VARIANTS: &[&str] = &["ZKML", "CIRCOM", "JOLT", "EZKL", "JSTPROVE"];

impl<'de> Deserialize<'de> for ProofSystem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProofSystemVisitor;

        impl<'de> Visitor<'de> for ProofSystemVisitor {
            type Value = ProofSystem;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a proof system variant")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "ZKML" => Ok(ProofSystem::ZKML),
                    "CIRCOM" => Ok(ProofSystem::CIRCOM),
                    "JOLT" => Ok(ProofSystem::JOLT),
                    "EZKL" => Ok(ProofSystem::EZKL),
                    "JSTPROVE" => Ok(ProofSystem::JSTPROVE),
                    _ => Err(E::unknown_variant(v, PROOF_SYSTEM_VARIANTS)),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                if let Some(key) = map.next_key::<String>()? {
                    let _: de::IgnoredAny = map.next_value()?;
                    self.visit_str(&key)
                } else {
                    Err(A::Error::custom("expected non-empty map for enum variant"))
                }
            }
        }

        deserializer.deserialize_any(ProofSystemVisitor)
    }
}

impl std::fmt::Display for ProofSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZKML => write!(f, "ZKML"),
            Self::CIRCOM => write!(f, "CIRCOM"),
            Self::JOLT => write!(f, "JOLT"),
            Self::EZKL => write!(f, "EZKL"),
            Self::JSTPROVE => write!(f, "JSTPROVE"),
        }
    }
}

impl FromStr for ProofSystem {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ZKML" => Ok(Self::ZKML),
            "CIRCOM" => Ok(Self::CIRCOM),
            "JOLT" => Ok(Self::JOLT),
            "EZKL" => Ok(Self::EZKL),
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
