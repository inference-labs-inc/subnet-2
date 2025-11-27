from __future__ import annotations
import json
import os
from dataclasses import dataclass
from typing import TYPE_CHECKING, Optional, Tuple

import bittensor as bt

from execution_layer.proof_handlers.base_handler import ProofSystemHandler
from execution_layer.generic_input import GenericInput

# Library-mode imports from dsperse
from dsperse.src.run.runner import Runner as DsperseRunner
from dsperse.src.prover import Prover as DsperseProver
from dsperse.src.verifier import Verifier as DsperseVerifier

if TYPE_CHECKING:
    from execution_layer.verified_model_session import VerifiedModelSession


@dataclass
class DsperseConfig:
    dslice_path: str          # Absolute path to the single slice .dslice file to operate on
    run_root: str             # Directory under which run_YYYYMMDD_HHMMSS/ folders are created


class DsperseHandler(ProofSystemHandler):
    """
    DSperse handler (library mode, per-slice):
    - generate_witness() → executes Runner.run on a single .dslice and writes a timestamped run directory
    - gen_proof()        → executes Prover.prove for that run and slice; returns proof.json contents
    - verify_proof()     → executes Verifier.verify for that run and slice; returns boolean

    Slicing/compilation are performed offline; each .dslice is expected to contain compiled EZKL artifacts.
    """

    # -------------
    # High-level API expected by the system
    # -------------

    def gen_input_file(self, session: VerifiedModelSession):
        bt.logging.trace("[DSperse] Generating input file")
        if isinstance(session.inputs.data, list):
            input_data = session.inputs.data
        else:
            input_data = session.inputs.to_array()
        data = {"input_data": input_data}
        os.makedirs(os.path.dirname(session.session_storage.input_path), exist_ok=True)
        with open(session.session_storage.input_path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        bt.logging.trace(f"[DSperse] Wrote input.json → {session.session_storage.input_path}")

    def generate_witness(
        self, session: VerifiedModelSession, return_content: bool = False
    ) -> list | dict:
        """
        Run DSperse runner for a single slice. Returns the created run directory or, if requested,
        the parsed run_results.json content.
        """
        cfg = self._resolve_config(session)
        self._ensure_dsperse_available()

        os.makedirs(cfg.run_root, exist_ok=True)
        bt.logging.debug(f"[DSperse] Running slice with runner: dslice={cfg.dslice_path} run_root={cfg.run_root}")
        runner = DsperseRunner(slice_path=cfg.dslice_path, run_metadata_path=None, save_metadata_path=None)

        # Runner.run accepts output_path as a root; it will create run_YYYYMMDD_HHMMSS under it
        runner.run(session.session_storage.input_path, output_path=cfg.run_root)

        # Select the latest run directory created under run_root
        run_dir = self._select_latest_run(cfg.run_root)
        if not run_dir:
            raise RuntimeError("[DSperse] No run directory found after runner.run()")

        if return_content:
            results_path = os.path.join(run_dir, "run_results.json")
            if os.path.exists(results_path):
                with open(results_path, "r", encoding="utf-8") as f:
                    return json.load(f)
            return {"run_dir": run_dir}
        return [run_dir]

    def gen_proof(self, session: VerifiedModelSession) -> Tuple[str, str]:
        """
        Generate a proof for the selected slice and latest run under run_root. Returns
        (proof_json_str, instances_json_str). If instances are not present, returns "[]" for instances.
        """
        cfg = self._resolve_config(session)
        self._ensure_dsperse_available()

        run_dir = self._select_latest_run(cfg.run_root)
        if not run_dir:
            raise RuntimeError("[DSperse] No run directory available for proving. Run generate_witness() first.")

        bt.logging.debug(f"[DSperse] Proving slice: run_dir={run_dir} dslice={cfg.dslice_path}")
        prover = DsperseProver()
        _ = prover.prove(run_dir, cfg.dslice_path)  # updates run_results.json and writes proof.json

        proof_path = self._locate_proof_json(run_dir)
        if not proof_path or not os.path.exists(proof_path):
            raise RuntimeError("[DSperse] Proof file not found after proving.")

        with open(proof_path, "r", encoding="utf-8") as f:
            proof_json = json.load(f)
        instances = proof_json.get("instances", [])
        return json.dumps(proof_json), json.dumps(instances)

    def verify_proof(
        self,
        session: VerifiedModelSession,
        validator_inputs: GenericInput,  # not used by DSperse verify
        proof: dict | str,               # not used; verify reads from run_dir + dslice
    ) -> bool:
        cfg = self._resolve_config(session)
        self._ensure_dsperse_available()

        run_dir = self._select_latest_run(cfg.run_root)
        if not run_dir:
            bt.logging.error("[DSperse] No run directory found for verification.")
            return False

        bt.logging.debug(f"[DSperse] Verifying slice: run_dir={run_dir} dslice={cfg.dslice_path}")
        verifier = DsperseVerifier()
        results = verifier.verify(run_dir, cfg.dslice_path)

        try:
            exec_chain = results.get("execution_chain", {})
            verified = int(exec_chain.get("ezkl_verified_slices", 0))
            proved = int(exec_chain.get("ezkl_proved_slices", 0)) or 1  # treat as 1 if single slice
            return verified >= min(1, proved)
        except Exception:
            # Fallback: check if any entry has verification_execution.verified truthy
            try:
                for entry in results.get("execution_chain", {}).get("execution_results", []):
                    ve = entry.get("verification_execution", {})
                    if ve and (ve.get("verified") or ve.get("success") or ve.get("success") is True):
                        return True
            except Exception:
                pass
            return False

    def aggregate_proofs(self, session: VerifiedModelSession, proofs: list[str]) -> tuple[str, float]:
        # Non-cryptographic aggregation: return a manifest of provided proof JSONs
        try:
            parts = [json.loads(p) if isinstance(p, str) else p for p in proofs]
        except Exception:
            parts = proofs
        manifest = {"type": "dsperse.aggregate", "parts": parts}
        return json.dumps(manifest), 0.0

    # -------------
    # Helpers
    # -------------

    def _resolve_config(self, session: VerifiedModelSession) -> DsperseConfig:
        """
        Resolve dslice_path and run_root from session.model.settings['dsperse'].
        Expected structure:
          session.model.settings = { ..., "dsperse": { "dslice_path": "/abs/path/to/slice_0.dslice",
                                                       "run_root": "/abs/path/to/model/run" } }
        Fallbacks use session.model.paths.root when present.
        """
        settings = getattr(session.model, "settings", {}) or {}
        dsp = settings.get("dsperse", {}) if isinstance(settings, dict) else {}

        dslice_path = dsp.get("dslice_path")
        run_root = dsp.get("run_root")

        root = getattr(session.model.paths, "root", None)

        if not dslice_path and root:
            # If only one dslice exists under <root>/slices, pick it
            slices_dir = os.path.join(root, "slices")
            if os.path.isdir(slices_dir):
                candidates = [os.path.join(slices_dir, f) for f in os.listdir(slices_dir) if f.endswith(".dslice")]
                if len(candidates) == 1:
                    dslice_path = candidates[0]

        if not run_root and root:
            run_root = os.path.join(root, "run")

        if not dslice_path or not run_root:
            raise RuntimeError(
                "[DSperse] Missing configuration: set session.model.settings['dsperse'] with 'dslice_path' and 'run_root'."
            )
        return DsperseConfig(dslice_path=os.path.abspath(dslice_path), run_root=os.path.abspath(run_root))

    def _select_latest_run(self, run_root: str) -> Optional[str]:
        if not os.path.isdir(run_root):
            return None
        runs = [os.path.join(run_root, d) for d in os.listdir(run_root) if d.startswith("run_")]
        runs = [d for d in runs if os.path.isdir(d)]
        if not runs:
            return None
        runs.sort(key=lambda p: os.path.getmtime(p), reverse=True)
        return runs[0]

    def _locate_proof_json(self, run_dir: str) -> Optional[str]:
        """Search under run_dir for slice_* subdirs containing proof.json and return the first found."""
        try:
            for name in sorted(os.listdir(run_dir)):
                if name.startswith("slice_"):
                    candidate = os.path.join(run_dir, name, "proof.json")
                    if os.path.exists(candidate):
                        return candidate
        except Exception:
            pass
        # As a fallback, consult run_results.json for recorded proof paths
        rr = os.path.join(run_dir, "run_results.json")
        if os.path.exists(rr):
            try:
                with open(rr, "r", encoding="utf-8") as f:
                    run_results = json.load(f)
                for entry in run_results.get("execution_chain", {}).get("execution_results", []):
                    pe = entry.get("proof_execution", {})
                    # different keys observed: proof_file or proof_path
                    path = pe.get("proof_file") or pe.get("proof_path")
                    if path and os.path.exists(path):
                        return path
            except Exception:
                pass
        return None

    def _ensure_dsperse_available(self) -> None:
        if DsperseRunner is None or DsperseProver is None or DsperseVerifier is None:
            raise ImportError(
                "DSperse library not available. Please ensure 'dsperse' is installed and importable."
            )

