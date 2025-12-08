# Competition Technical Guide

## Overview

This guide provides a deep technical dive into participating in competitions as a miner. Competitions are a mechanism for miners to submit optimized zero-knowledge circuits that prove neural network execution, with rewards based on circuit performance across multiple metrics.

## Circuit Evaluation

The scoring system evaluates circuits based on accuracy (40% weight), proof size (30% weight), and response time (30% weight). Accuracy measures how closely circuit outputs match the baseline model using MSE loss and exponential transformation. Proof size evaluates the compactness of generated zero-knowledge proofs relative to current SOTA. Response time measures proof generation speed normalized against SOTA performance.

The final score calculation uses an exponential decay formula that creates a score between 0 and 1, where higher scores indicate better performance relative to the current SOTA. The formula penalizes poor performance exponentially, encouraging continuous improvement and optimization:

```
score = exp(-(
    0.4 * max(0, sota_accuracy - accuracy) +
    0.3 * max(0, (proof_size - sota_proof_size)/sota_proof_size) +
    0.3 * max(0, (response_time - sota_response_time)/sota_response_time)
))
```

## Technical Requirements

Your circuit must process inputs matching the competition config shape and produce a matching output shape.

The submission package must include a compiled circuit (`model.compiled`) and a `settings.json` configuration file. Validators will generate proving and verification keys locally during evaluation via `ezkl setup` within a 5-minute, memory-capped setup step.

## Evaluation Process

The evaluation process runs through multiple rounds of testing to ensure consistent performance. Each round generates random test inputs that are fed through both your circuit and a baseline model. The baseline comparison uses either PyTorch or ONNX models, supporting flexible implementation approaches.

Your circuit must generate valid proofs that verify successfully. The system measures proof generation time and size across 10 evaluation rounds, averaging the metrics to determine final scores. All verifications must pass for a valid submission - a single failure results in disqualification.

## Deployment Architecture

The competition system uses cloud storage (R2/S3) for circuit file management. When validators request your circuit, they receive signed URLs for secure file access.

The commitment process anchors the SHA256 hash of your `model.compiled` on-chain. This creates an immutable record of your submission and prevents tampering. The system verifies that local and chain commitments match before proceeding with evaluation.

## Optimization Guidelines

Circuit optimization requires balancing multiple competing factors. Reducing circuit complexity generally improves proof generation speed and size but may impact accuracy. The scoring formula's weights guide this tradeoff - accuracy carries the highest weight at 40%.

Resource management plays a crucial role in performance. Proof generation demands significant GPU power and memory. Monitor system resources during testing to ensure your circuit operates within validator timeout limits. Profile your operations to identify and eliminate bottlenecks.

## Platform Requirements

Currently, validators run using macOS arm64 architecture. This requirement ensures consistent evaluation environments across all participants. While you can develop and test on other platforms, final submissions must be validated on the required architecture to maintain consensus and provide the most optimal benchmark for the use case.
