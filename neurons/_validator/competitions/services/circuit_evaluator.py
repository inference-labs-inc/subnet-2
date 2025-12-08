import os
import json
import time
import tempfile
import subprocess
import torch
import numpy as np
import bittensor as bt
from typing import Tuple, List, Optional
from constants import LOCAL_EZKL_PATH, FIVE_MINUTES
from _validator.competitions.services.sota_manager import SotaManager
from _validator.competitions.services.data_source import (
    CompetitionDataSource,
    RandomDataSource,
    RemoteDataSource,
)
from utils.wandb_logger import safe_log
import shutil
import traceback
from _validator.competitions.services.data_source import (
    CompetitionDataProcessor,
)
import logging
from utils.system import get_temp_folder
import sys
import random

logging.getLogger("onnxruntime").setLevel(logging.ERROR)
os.environ["ONNXRUNTIME_LOGGING_LEVEL"] = "3"


class CircuitEvaluator:
    def __init__(
        self,
        config: dict,
        competition_directory: str,
        sota_manager: SotaManager,
    ):

        self.competition_directory = competition_directory
        self.sota_manager = sota_manager

        self.baseline_model = os.path.join(
            self.competition_directory, config["baseline_model_path"]
        )
        bt.logging.debug(f"Using baseline model at: {self.baseline_model}")

        self.is_onnx = not isinstance(self.baseline_model, torch.nn.Module)

        self.onnx_venv = os.path.abspath(
            os.path.join(competition_directory, "onnx_venv")
        )
        self.onnx_runner = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "onnx_runner.py"
        )

        if self.is_onnx:
            if not os.path.exists(self.onnx_venv):
                if not self._setup_onnx_env():
                    raise RuntimeError("Failed to set up ONNX environment")
            else:
                bt.logging.debug(f"Using existing ONNX environment at {self.onnx_venv}")

        self.data_source = self._setup_data_source()

    def _ensure_keys(self, circuit_dir: str) -> bool:
        try:
            model_path = os.path.join(circuit_dir, "model.compiled")
            settings_path = os.path.join(circuit_dir, "settings.json")
            vk_path = os.path.join(circuit_dir, "vk.key")
            pk_path = os.path.join(circuit_dir, "pk.key")
            if os.path.exists(vk_path) and os.path.exists(pk_path):
                return True
            if not os.path.exists(model_path) or not os.path.exists(settings_path):
                return False

            def set_limits():
                try:
                    import resource

                    limit_gb = int(os.getenv("SUBNET_2_EZKL_SETUP_MEMORY_GB", "16"))
                    limit_bytes = limit_gb * 1024 * 1024 * 1024
                    resource.setrlimit(resource.RLIMIT_AS, (limit_bytes, limit_bytes))
                except Exception:
                    pass

            result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "setup",
                    "--compiled-circuit",
                    model_path,
                    "--vk-path",
                    vk_path,
                    "--pk-path",
                    pk_path,
                ],
                capture_output=True,
                text=True,
                timeout=FIVE_MINUTES,
                preexec_fn=set_limits,
            )
            if result.returncode != 0:
                bt.logging.error(f"EZKL setup failed: {result.stderr}")
                return False
            return os.path.exists(vk_path) and os.path.exists(pk_path)
        except subprocess.TimeoutExpired:
            bt.logging.error("EZKL setup timed out")
            return False
        except Exception as e:
            bt.logging.error(f"Error during EZKL setup: {e}")
            return False

    def _get_python_path(self):
        python_path = os.path.join(self.onnx_venv, "bin", "python")
        if os.path.exists(python_path):
            return python_path

        python_path = os.path.join(self.onnx_venv, "bin", "python3")
        if os.path.exists(python_path):
            return python_path

        for root, dirs, files in os.walk(self.onnx_venv):
            for file in files:
                if file == "python" or file == "python3":
                    return os.path.join(root, file)

        return None

    def _get_pip_path(self):
        pip_path = os.path.join(self.onnx_venv, "bin", "pip")
        if os.path.exists(pip_path):
            return pip_path

        pip_path = os.path.join(self.onnx_venv, "bin", "pip3")
        if os.path.exists(pip_path):
            return pip_path

        for root, dirs, files in os.walk(self.onnx_venv):
            for file in files:
                if file == "pip" or file == "pip3":
                    return os.path.join(root, file)

        return None

    def _find_site_packages(self):
        site_packages = None
        for root, dirs, files in os.walk(self.onnx_venv):
            if os.path.basename(root) == "site-packages":
                site_packages = root
                break

        if not site_packages:
            python_path = self._get_python_path()
            if python_path:
                try:
                    python_version_result = subprocess.run(
                        [python_path, "--version"],
                        capture_output=True,
                        text=True,
                    )
                    python_version_output = python_version_result.stdout.strip()
                    if not python_version_output:
                        python_version_output = python_version_result.stderr.strip()
                    python_version = python_version_output.split()[1][:3]
                    site_packages = os.path.join(
                        self.onnx_venv,
                        "lib",
                        f"python{python_version}",
                        "site-packages",
                    )
                except Exception:
                    site_packages = os.path.join(
                        self.onnx_venv, "lib", "python3.10", "site-packages"
                    )
            else:
                site_packages = os.path.join(
                    self.onnx_venv, "lib", "python3.10", "site-packages"
                )

        if not os.path.exists(site_packages):
            os.makedirs(site_packages, exist_ok=True)

        return site_packages

    def _get_runner_path(self):
        runner_path = None
        for root, dirs, files in os.walk(self.onnx_venv):
            if "onnx_runner.py" in files:
                runner_path = os.path.join(root, "onnx_runner.py")
                break

        if not runner_path or not os.path.exists(runner_path):
            site_packages = self._find_site_packages()
            runner_path = os.path.join(site_packages, "onnx_runner.py")
            if not os.path.exists(runner_path):
                shutil.copy2(self.onnx_runner, runner_path)

        if not os.path.exists(runner_path):
            runner_path = self.onnx_runner

        return runner_path

    def _setup_onnx_env(self):
        try:
            parent_dir = os.path.dirname(self.onnx_venv)
            if not os.path.exists(parent_dir):
                bt.logging.error(f"Parent directory {parent_dir} does not exist")
                return False

            if not os.access(parent_dir, os.W_OK):
                bt.logging.error(f"No write permission for directory {parent_dir}")
                return False

            os.makedirs(self.onnx_venv, exist_ok=True)
            bt.logging.debug(f"Creating ONNX venv at {self.onnx_venv}")

            result = subprocess.run(
                [sys.executable, "-m", "venv", self.onnx_venv],
                capture_output=True,
                text=True,
            )

            if result.returncode != 0:
                bt.logging.error(
                    f"Failed to create venv using {sys.executable}: {result.stderr}"
                )
                return False

            python_path = self._get_python_path()
            if not python_path:
                bt.logging.error(
                    f"Python not found in venv at {self.onnx_venv} after creation"
                )
                return False

            bt.logging.debug(f"Found Python at {python_path}")

            python_version_result = subprocess.run(
                [python_path, "--version"],
                capture_output=True,
                text=True,
            )

            if python_version_result.returncode != 0:
                bt.logging.error(
                    f"Failed to get Python version: {python_version_result.stderr}"
                )
                return False

            python_version_output = python_version_result.stdout.strip()
            if not python_version_output:
                python_version_output = python_version_result.stderr.strip()

            python_version = python_version_output.split()[1][:3]
            bt.logging.debug(f"Python version: {python_version}")

            pip_path = self._get_pip_path()
            if not pip_path:
                bt.logging.error(f"Pip not found in venv at {self.onnx_venv}")
                return False

            bt.logging.debug(f"Found pip at {pip_path}")

            pip_result = subprocess.run(
                [pip_path, "install", "numpy", "onnxruntime==1.20.1"],
                capture_output=True,
                text=True,
            )

            if pip_result.returncode != 0:
                bt.logging.error(f"Failed to install dependencies: {pip_result.stderr}")
                return False

            site_packages = self._find_site_packages()
            bt.logging.debug(f"Using site-packages at {site_packages}")

            runner_path = os.path.join(site_packages, "onnx_runner.py")
            shutil.copy2(self.onnx_runner, runner_path)
            bt.logging.debug(f"Copied onnx_runner.py to {runner_path}")

            bt.logging.success(
                f"ONNX environment set up at {self.onnx_venv} with Python {python_version}"
            )
            return True
        except Exception as e:
            bt.logging.error(f"Error setting up ONNX environment: {str(e)}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return False

    def _setup_data_source(self) -> CompetitionDataSource:
        try:
            config_path = os.path.join(
                self.competition_directory, "competition_config.json"
            )
            bt.logging.info(f"Loading competition config from {config_path}")
            with open(config_path) as f:
                config = json.load(f)
                data_config = config.get("data_source", {})
                bt.logging.info(f"Data source config: {data_config}")

                processor = None
                processor_path = os.path.join(
                    self.competition_directory, "data_processor.py"
                )
                bt.logging.info(f"Looking for data processor at {processor_path}")
                if os.path.exists(processor_path):
                    bt.logging.info("Found data processor, loading module...")
                    import importlib.util

                    spec = importlib.util.spec_from_file_location(
                        "data_processor", processor_path
                    )
                    module = importlib.util.module_from_spec(spec)
                    spec.loader.exec_module(module)

                    for attr_name in dir(module):
                        attr = getattr(module, attr_name)
                        if (
                            isinstance(attr, type)
                            and issubclass(attr, CompetitionDataProcessor)
                            and attr != CompetitionDataProcessor
                        ):
                            processor = attr()
                            bt.logging.info(f"Loaded data processor: {attr.__name__}")
                            break

                if data_config.get("type") == "remote":
                    bt.logging.info("Initializing remote data source")
                    data_source = RemoteDataSource(
                        self.competition_directory, processor
                    )
                    if not data_source.sync_data():
                        bt.logging.warning(
                            "Failed to sync remote dataset; falling back to randomized data for evaluation."
                        )
                        return RandomDataSource(self.competition_directory, processor)
                    bt.logging.info("Successfully initialized remote data source")
                    return data_source
                bt.logging.info("Using random data source")
                return RandomDataSource(self.competition_directory, processor)
        except Exception as e:
            bt.logging.error(f"Error setting up data source: {e}")
            traceback.print_exc()
            bt.logging.warning(
                "Falling back to random data source due to critical error in setup."
            )
            return RandomDataSource(
                self.competition_directory, processor=getattr(self, "processor", None)
            )

    def _generate_witness_and_get_outputs(
        self, circuit_dir: str, test_inputs: torch.Tensor
    ) -> Tuple[Optional[List[float]], float, Optional[str]]:
        """
        Generates a witness and extracts rescaled outputs.
        Returns:
            - A list of public signals (rescaled outputs) or None if failed.
            - Time taken for witness generation.
            - Path to the witness file.
        """
        witness_gen_start_time = time.perf_counter()
        public_signals = None
        temp_witness_path = None

        try:
            input_data = {
                "input_data": [[float(x) for x in test_inputs.flatten().tolist()]]
            }

            with tempfile.NamedTemporaryFile(
                mode="w+", suffix=".json", dir=get_temp_folder(), delete=False
            ) as temp_input:
                json.dump(input_data, temp_input, indent=2)
                temp_input_path = temp_input.name

            with tempfile.NamedTemporaryFile(
                mode="w+", suffix=".json", dir=get_temp_folder(), delete=False
            ) as temp_witness:
                temp_witness_path = temp_witness.name

            model_path = os.path.join(circuit_dir, "model.compiled")
            if not os.path.exists(model_path):
                bt.logging.error(f"model.compiled not found at {model_path}")
                os.unlink(temp_input_path)
                if temp_witness_path and os.path.exists(temp_witness_path):
                    os.unlink(temp_witness_path)
                return None, time.perf_counter() - witness_gen_start_time, None

            bt.logging.debug(
                f"Witness-Only: Input data: {json.dumps(input_data, indent=2)}"
            )
            witness_result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "gen-witness",
                    "--data",
                    temp_input_path,
                    "--compiled-circuit",
                    model_path,
                    "--output",
                    temp_witness_path,
                ],
                capture_output=True,
                text=True,
                timeout=300,
            )

            os.unlink(temp_input_path)

            if witness_result.returncode != 0:
                bt.logging.error(
                    f"Witness-Only: Witness generation failed with code {witness_result.returncode}"
                )
                bt.logging.error(f"STDOUT: {witness_result.stdout}")
                bt.logging.error(f"STDERR: {witness_result.stderr}")
                if temp_witness_path and os.path.exists(temp_witness_path):
                    os.unlink(temp_witness_path)
                return None, time.perf_counter() - witness_gen_start_time, None

            try:
                with open(temp_witness_path, "r") as f_wit:
                    witness_content = json.load(f_wit)

                pretty_elements_dict = witness_content.get("pretty_elements", {})
                rescaled_outputs_nested = pretty_elements_dict.get(
                    "rescaled_outputs", []
                )
                public_signals = [
                    float(x) for sublist in rescaled_outputs_nested for x in sublist
                ]

            except Exception as e:
                bt.logging.warning(
                    f"Witness-Only: Could not parse outputs from witness file {temp_witness_path}: {e}"
                )
                public_signals = None

            if not public_signals:
                bt.logging.warning(
                    f"Witness-Only: Failed to extract public signals from witness: {temp_witness_path}"
                )
                return (
                    None,
                    time.perf_counter() - witness_gen_start_time,
                    temp_witness_path,
                )

            bt.logging.debug(
                f"Witness-Only: Successfully extracted signals from witness {temp_witness_path}"
            )
            return (
                public_signals,
                time.perf_counter() - witness_gen_start_time,
                temp_witness_path,
            )

        except Exception as e:
            bt.logging.error(
                f"Witness-Only: Error in witness generation and output extraction: {e}"
            )
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            if temp_witness_path and os.path.exists(temp_witness_path):
                try:
                    os.unlink(temp_witness_path)
                except OSError as ose:
                    bt.logging.warning(
                        f"Could not unlink temporary witness file {temp_witness_path}: {ose}"
                    )

            return None, time.perf_counter() - witness_gen_start_time, None

    def evaluate(self, circuit_dir: str) -> Tuple[float, float, float, bool, float]:
        (
            raw_accuracy_scores,
            proof_sizes_collected,
            response_times_collected,
            verification_results_collected,
        ) = (
            [],
            [],
            [],
            [],
        )
        all_collected_outputs_for_constancy_check = []

        if not self._ensure_keys(circuit_dir):
            bt.logging.error("Failed to prepare proving and verification keys")
            return 0.0, float("inf"), float("inf"), False, 0.0
        input_shape = self._get_input_shape(circuit_dir)
        bt.logging.debug(f"Got input shape: {input_shape}")

        if not input_shape:
            bt.logging.error("Failed to get input shape")
            return 0.0, float("inf"), float("inf"), False, 0.0

        try:
            with open(
                os.path.join(self.competition_directory, "competition_config.json")
            ) as f:
                config = json.load(f)
                num_total_evaluations = config["evaluation"].get(
                    "num_total_evaluations", 100
                )
                num_proof_evaluations = config["evaluation"].get(
                    "num_proof_evaluations", 10
                )
                if num_proof_evaluations > num_total_evaluations:
                    bt.logging.warning(
                        "num_proof_evaluations cannot exceed num_total_evaluations. Setting to num_total_evaluations."
                    )
                    num_proof_evaluations = num_total_evaluations
        except Exception as e:
            bt.logging.error(
                f"Error loading evaluation counts from config, using defaults: {e}"
            )
            num_total_evaluations = 100
            num_proof_evaluations = 10

        bt.logging.info(
            f"Starting evaluation: {num_total_evaluations} total, {num_proof_evaluations} with full proof."
        )

        if num_total_evaluations > 0 and num_proof_evaluations > 0:
            proof_iteration_indices = sorted(
                random.sample(range(num_total_evaluations), num_proof_evaluations)
            )
        else:
            proof_iteration_indices = []

        bt.logging.debug(
            f"Iterations selected for full proof: {proof_iteration_indices}"
        )

        first_valid_output_tensor = None
        all_outputs_identical = True
        successful_valid_outputs_count = 0

        for i in range(num_total_evaluations):
            bt.logging.debug(f"Running evaluation {i + 1}/{num_total_evaluations}")
            iteration_is_full_proof = i in proof_iteration_indices
            witness_file_for_this_iter = None

            try:
                test_inputs = self.data_source.get_benchmark_data()
                if test_inputs is None:
                    bt.logging.error("Failed to get benchmark data for iteration")
                    raw_accuracy_scores.append(0.0)
                    if iteration_is_full_proof:
                        verification_results_collected.append(False)
                    continue

                bt.logging.debug(f"Got benchmark data with shape: {test_inputs.shape}")

                baseline_output = self._run_baseline_model(test_inputs)
                if baseline_output is None:
                    bt.logging.error("Baseline model run failed for iteration")
                    raw_accuracy_scores.append(0.0)
                    if iteration_is_full_proof:
                        verification_results_collected.append(False)
                    continue

                bt.logging.debug(
                    f"Got baseline output with shape: {np.array(baseline_output).shape}"
                )

                current_output_signals = None
                raw_accuracy_this_iter = 0.0

                if iteration_is_full_proof:
                    bt.logging.debug(f"Iteration {i + 1} is a proof evaluation.")
                    proof_result = self._generate_proof(circuit_dir, test_inputs)
                    if not proof_result:
                        bt.logging.error("Full Proof: Proof generation failed")
                        raw_accuracy_scores.append(0.0)
                        verification_results_collected.append(False)
                        proof_sizes_collected.append(float("inf"))
                        response_times_collected.append(float("inf"))
                        continue

                    proof_path, proof_data, response_time = proof_result
                    response_times_collected.append(response_time)

                    proof_bytes = proof_data.get("proof", [])
                    public_signals_from_proof = [
                        float(x)
                        for sublist in proof_data.get("pretty_public_inputs", {}).get(
                            "rescaled_outputs", []
                        )
                        for x in sublist
                    ]
                    proof_sizes_collected.append(len(proof_bytes))
                    current_output_signals = public_signals_from_proof

                    verify_result = self._verify_proof(circuit_dir, proof_path)
                    bt.logging.debug(
                        f"Full Proof: Proof verification result: {verify_result}"
                    )
                    verification_results_collected.append(verify_result)

                    if verify_result:
                        raw_accuracy_this_iter = self._compare_outputs(
                            baseline_output, public_signals_from_proof
                        )
                        bt.logging.debug(
                            f"Full Proof: Raw accuracy: {raw_accuracy_this_iter}"
                        )
                    else:
                        bt.logging.error("Full Proof: Proof verification failed")
                        raw_accuracy_this_iter = 0.0
                    raw_accuracy_scores.append(raw_accuracy_this_iter)

                else:
                    bt.logging.debug(f"Iteration {i + 1} is a witness only evaluation.")
                    witness_outputs, witness_gen_time, witness_file_path = (
                        self._generate_witness_and_get_outputs(circuit_dir, test_inputs)
                    )

                    witness_file_for_this_iter = witness_file_path

                    if witness_outputs is not None:
                        current_output_signals = witness_outputs
                        raw_accuracy_this_iter = self._compare_outputs(
                            baseline_output, witness_outputs
                        )
                        bt.logging.debug(
                            f"Witness-Only: Raw accuracy: {raw_accuracy_this_iter}"
                        )
                    else:
                        bt.logging.error(
                            "Witness-Only: Failed to get outputs from witness"
                        )
                        raw_accuracy_this_iter = 0.0
                    raw_accuracy_scores.append(raw_accuracy_this_iter)

                if current_output_signals is not None:
                    all_collected_outputs_for_constancy_check.append(
                        torch.tensor(current_output_signals)
                    )
                    current_output_tensor_for_check = torch.tensor(
                        current_output_signals
                    )
                    if first_valid_output_tensor is None:
                        first_valid_output_tensor = current_output_tensor_for_check
                        successful_valid_outputs_count += 1
                    elif all_outputs_identical:
                        successful_valid_outputs_count += 1
                        if not torch.allclose(
                            first_valid_output_tensor,
                            current_output_tensor_for_check,
                            atol=1e-4,
                        ):
                            all_outputs_identical = False

                if witness_file_for_this_iter and os.path.exists(
                    witness_file_for_this_iter
                ):
                    try:
                        os.unlink(witness_file_for_this_iter)
                        bt.logging.debug(
                            f"Cleaned up temp witness file: {witness_file_for_this_iter}"
                        )
                    except OSError as ose:
                        bt.logging.warning(
                            f"Could not unlink temp witness file {witness_file_for_this_iter} post-iteration: {ose}"
                        )

            except Exception as e:
                bt.logging.error(f"Error in evaluation iteration {i + 1}: {str(e)}")
                bt.logging.error(f"Stack trace: {traceback.format_exc()}")
                raw_accuracy_scores.append(0.0)
                if iteration_is_full_proof:
                    verification_results_collected.append(False)
                    proof_sizes_collected.append(float("inf"))
                    response_times_collected.append(float("inf"))
                if witness_file_for_this_iter and os.path.exists(
                    witness_file_for_this_iter
                ):
                    try:
                        os.unlink(witness_file_for_this_iter)
                    except OSError as ose:
                        bt.logging.warning(
                            f"Could not unlink temp witness file "
                            f"{witness_file_for_this_iter} during exception handling: {ose}"
                        )

        if not proof_iteration_indices:
            overall_verification_successful = False
            bt.logging.warning(
                "No full proof iterations were scheduled. Overall verification defaults to False."
            )
        elif not verification_results_collected:
            overall_verification_successful = False
            bt.logging.warning(
                "No verification results from proof iterations. Overall verification defaults to False."
            )
        else:
            overall_verification_successful = all(
                v for v in verification_results_collected if isinstance(v, bool)
            )

        if not overall_verification_successful:
            bt.logging.error(
                "One or more required verifications (from full proof set) failed - setting all scores to 0"
            )
            return 0.0, float("inf"), float("inf"), False, 0.0

        if successful_valid_outputs_count > 1 and all_outputs_identical:
            bt.logging.warning(
                f"Detected constant output from circuit across "
                f"{successful_valid_outputs_count} valid inputs. Penalizing accuracy."
            )
            raw_accuracy_scores = [0.0] * len(raw_accuracy_scores)

        avg_raw_accuracy = (
            sum(raw_accuracy_scores) / len(raw_accuracy_scores)
            if raw_accuracy_scores
            else 0
        )
        valid_proof_sizes = [ps for ps in proof_sizes_collected if ps != float("inf")]
        avg_proof_size = (
            sum(valid_proof_sizes) / len(valid_proof_sizes)
            if valid_proof_sizes
            else float("inf")
        )
        valid_response_times = [
            rt for rt in response_times_collected if rt != float("inf")
        ]
        avg_response_time = (
            sum(valid_response_times) / len(valid_response_times)
            if valid_response_times
            else float("inf")
        )

        successful_accuracy_iterations = len([s for s in raw_accuracy_scores if s > 0])
        num_actual_proof_attempts = len(verification_results_collected)

        safe_log_payload = {
            "avg_raw_accuracy": float(avg_raw_accuracy),
            "avg_proof_size": (
                float(avg_proof_size) if avg_proof_size != float("inf") else -1
            ),
            "avg_response_time": (
                float(avg_response_time) if avg_response_time != float("inf") else -1
            ),
            "total_eval_iterations": num_total_evaluations,
            "num_proof_iterations_scheduled": num_proof_evaluations,
            "num_proof_iterations_attempted": num_actual_proof_attempts,
            "successful_accuracy_iterations": successful_accuracy_iterations,
            "verification_success_rate_on_proof_subset": (
                sum(v for v in verification_results_collected if v is True)
                / max(num_actual_proof_attempts, 1)
                if num_actual_proof_attempts > 0
                else 0
            ),
            "constant_output_detected": successful_valid_outputs_count > 1
            and all_outputs_identical,
            "raw_accuracy_distribution": [
                float(f"{x:.4f}") for x in raw_accuracy_scores
            ],
            "proof_sizes_distribution_from_proof_subset": [
                float(x) if x != float("inf") else -1 for x in proof_sizes_collected
            ],
            "response_times_distribution_from_proof_subset": [
                float(f"{x:.2f}") if x != float("inf") else -1
                for x in response_times_collected
            ],
        }
        safe_log(safe_log_payload)

        bt.logging.info(
            f"Circuit evaluation complete - Avg Raw Accuracy (all iters): {avg_raw_accuracy:.4f}, "
            f"Avg Proof Size (proof subset): {avg_proof_size:.0f}, "
            f"Avg Response Time (proof subset): {avg_response_time:.2f}s. "
            f"Overall Verification (proof subset): {overall_verification_successful}"
        )

        return (
            avg_raw_accuracy,
            avg_proof_size,
            avg_response_time,
            overall_verification_successful,
            avg_raw_accuracy,
        )

    def _get_input_shape(self, circuit_dir: str) -> Tuple[int, int] | None:
        try:
            config_path = os.path.join(
                self.competition_directory, "competition_config.json"
            )
            bt.logging.debug(f"Reading config from: {config_path}")
            with open(config_path) as f:
                config = json.load(f)
                if (
                    "circuit_settings" in config
                    and "input_shape" in config["circuit_settings"]
                ):
                    return tuple(config["circuit_settings"]["input_shape"])
            return None
        except Exception as e:
            bt.logging.error(f"Error reading input shape: {e}")
            return None

    def _run_baseline_model(self, test_inputs: torch.Tensor) -> List | None:
        try:
            if not self.is_onnx:
                return self.baseline_model(test_inputs).tolist()

            python_path = self._get_python_path()
            if not python_path:
                bt.logging.warning(
                    f"Python not found in ONNX venv at {self.onnx_venv}, attempting to recreate"
                )
                if not self._setup_onnx_env():
                    bt.logging.error("Failed to recreate ONNX environment")
                    return None

                python_path = self._get_python_path()
                if not python_path:
                    bt.logging.error(
                        "Still couldn't find Python after recreating environment"
                    )
                    return None

            with (
                tempfile.NamedTemporaryFile(suffix=".npy") as input_file,
                tempfile.NamedTemporaryFile(suffix=".npy") as output_file,
            ):
                np.save(input_file.name, test_inputs.numpy())
                model_path = os.path.abspath(self.baseline_model)

                bt.logging.debug(f"ONNX model path: {model_path}")
                bt.logging.debug(f"Input shape: {test_inputs.shape}")
                bt.logging.debug(f"Input file: {input_file.name}")
                bt.logging.debug(f"Output file: {output_file.name}")
                bt.logging.debug(f"Using Python at: {python_path}")

                if not os.path.exists(model_path):
                    bt.logging.error(f"ONNX model not found at {model_path}")
                    return None

                runner_path = self._get_runner_path()
                bt.logging.debug(f"Using runner at: {runner_path}")

                process = subprocess.Popen(
                    [
                        python_path,
                        runner_path,
                        model_path,
                        input_file.name,
                        output_file.name,
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    bufsize=1,
                    universal_newlines=True,
                )

                stdout, stderr = process.communicate()

                if process.returncode != 0:
                    bt.logging.error(
                        f"ONNX runner failed with code {process.returncode}"
                    )
                    bt.logging.error(f"STDOUT: {stdout}")
                    bt.logging.error(f"STDERR: {stderr}")
                    return None

                output = np.load(output_file.name)
                output_list = output.flatten().tolist()
                return output_list
        except Exception as e:
            bt.logging.error(f"Error running baseline model: {e}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return None

    def _generate_proof(
        self, circuit_dir: str, test_inputs: torch.Tensor
    ) -> Tuple[str, dict, float] | None:
        try:
            input_data = {
                "input_data": [[float(x) for x in test_inputs.flatten().tolist()]]
            }

            temp_input_path = None
            witness_path = None
            temp_proof_path = None

            temp_input_obj = tempfile.NamedTemporaryFile(
                mode="w+", suffix=".json", dir=get_temp_folder(), delete=False
            )
            temp_input_path = temp_input_obj.name
            json.dump(input_data, temp_input_obj, indent=2)
            temp_input_obj.close()

            temp_witness_obj = tempfile.NamedTemporaryFile(
                mode="w+", suffix=".json", dir=get_temp_folder(), delete=False
            )
            witness_path = temp_witness_obj.name
            temp_witness_obj.close()

            temp_proof_obj = tempfile.NamedTemporaryFile(
                mode="w+", suffix=".json", dir=get_temp_folder(), delete=False
            )
            temp_proof_path = temp_proof_obj.name
            temp_proof_obj.close()

            model_path = os.path.join(circuit_dir, "model.compiled")
            if not os.path.exists(model_path):
                bt.logging.error(f"model.compiled not found at {model_path}")
                if temp_input_path and os.path.exists(temp_input_path):
                    os.unlink(temp_input_path)
                if witness_path and os.path.exists(witness_path):
                    os.unlink(witness_path)
                if temp_proof_path and os.path.exists(temp_proof_path):
                    os.unlink(temp_proof_path)
                return None

            bt.logging.debug(
                f"Full Proof: Input data: {json.dumps(input_data, indent=2)}"
            )
            witness_start_time = time.perf_counter()
            witness_result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "gen-witness",
                    "--data",
                    temp_input_path,
                    "--compiled-circuit",
                    model_path,
                    "--output",
                    witness_path,
                ],
                capture_output=True,
                text=True,
                timeout=300,
            )
            witness_time = time.perf_counter() - witness_start_time

            if witness_result.returncode != 0:
                bt.logging.error(
                    f"Full Proof: Witness generation failed with code {witness_result.returncode}"
                )
                bt.logging.error(f"STDOUT: {witness_result.stdout}")
                bt.logging.error(f"STDERR: {witness_result.stderr}")
                if temp_input_path and os.path.exists(temp_input_path):
                    os.unlink(temp_input_path)
                if witness_path and os.path.exists(witness_path):
                    os.unlink(witness_path)
                if temp_proof_path and os.path.exists(temp_proof_path):
                    os.unlink(temp_proof_path)
                return None

            os.unlink(temp_input_path)
            temp_input_path = None

            bt.logging.debug(
                "Full Proof: Witness generation successful, starting proof generation"
            )
            proof_gen_start_time = time.perf_counter()
            prove_result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "prove",
                    "--compiled-circuit",
                    model_path,
                    "--witness",
                    witness_path,
                    "--pk-path",
                    os.path.join(circuit_dir, "pk.key"),
                    "--proof-path",
                    temp_proof_path,
                ],
                capture_output=True,
                text=True,
                timeout=300,
            )
            proof_gen_time = time.perf_counter() - proof_gen_start_time

            os.unlink(witness_path)
            witness_path = None

            if prove_result.returncode != 0:
                bt.logging.error(
                    f"Full Proof: Proof generation failed with code {prove_result.returncode}"
                )
                bt.logging.error(f"STDOUT: {prove_result.stdout}")
                bt.logging.error(f"STDERR: {prove_result.stderr}")
                if temp_proof_path and os.path.exists(temp_proof_path):
                    os.unlink(temp_proof_path)
                return None

            with open(temp_proof_path) as f:
                proof_data = json.load(f)

            total_response_time = witness_time + proof_gen_time
            bt.logging.debug(
                f"Full Proof: Timing - Witness: {witness_time:.3f}s,"
                f" Proof: {proof_gen_time:.3f}s, Total: {total_response_time:.3f}s"
            )
            return temp_proof_path, proof_data, total_response_time
        except Exception as e:
            bt.logging.error(f"Full Proof: Error generating proof: {e}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            if temp_input_path and os.path.exists(temp_input_path):
                try:
                    os.unlink(temp_input_path)
                except OSError:
                    bt.logging.warning(
                        f"Failed to delete temp input path: {temp_input_path}"
                    )
            if witness_path and os.path.exists(witness_path):
                try:
                    os.unlink(witness_path)
                except OSError:
                    bt.logging.warning(f"Failed to delete witness path: {witness_path}")
            if temp_proof_path and os.path.exists(temp_proof_path):
                try:
                    os.unlink(temp_proof_path)
                except OSError:
                    bt.logging.warning(
                        f"Failed to delete temp proof path: {temp_proof_path}"
                    )
            return None

    def _verify_proof(self, circuit_dir: str, proof_path: str) -> bool:
        try:
            verify_result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "verify",
                    "--proof-path",
                    proof_path,
                    "--settings-path",
                    os.path.join(circuit_dir, "settings.json"),
                    "--vk-path",
                    os.path.join(circuit_dir, "vk.key"),
                ],
                capture_output=True,
                text=True,
                timeout=300,
            )
            return verify_result.returncode == 0
        except Exception as e:
            bt.logging.error(f"Error verifying proof: {e}")
            return False
        finally:
            if os.path.exists(proof_path):
                os.unlink(proof_path)

    def _compare_outputs(self, expected: list[float], actual: list[float]) -> float:
        try:
            with open(
                os.path.join(self.competition_directory, "competition_config.json")
            ) as f:
                config = json.load(f)
                output_shapes = config["circuit_settings"]["output_shapes"]
                total_size = sum(np.prod(shape) for shape in output_shapes.values())

            if isinstance(actual, dict) and "pretty_public_inputs" in actual:
                rescaled = actual["pretty_public_inputs"].get("rescaled_outputs", [])
                actual = [float(x) for sublist in rescaled for x in sublist]
            elif isinstance(actual, list):
                if len(actual) > 0 and isinstance(actual[0], list):
                    actual = [float(x) for sublist in actual for x in sublist]

            if len(actual) != total_size:
                bt.logging.error(
                    f"Output size mismatch: expected {total_size}, got {len(actual)}"
                )
                return 0.0

            expected = expected[:total_size]
            expected_tensor = torch.tensor(expected)
            actual_tensor = torch.tensor(actual)
            mse = torch.nn.functional.mse_loss(
                actual_tensor, expected_tensor, reduction="sum"
            )
            raw_accuracy = torch.exp(-mse).item()
            return raw_accuracy
        except Exception as e:
            bt.logging.error(f"Error comparing outputs: {e}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return 0.0

    def _run_onnx_model(
        self, model_path: str, inputs: np.ndarray, output_path: str
    ) -> Optional[np.ndarray]:
        try:
            python_path = self._get_python_path()
            if not python_path:
                bt.logging.error(f"Python not found in ONNX venv at {self.onnx_venv}")
                return None

            runner_path = self._get_runner_path()
            input_path = output_path + ".input.npy"
            np.save(input_path, inputs)

            bt.logging.debug(f"Running ONNX model with Python at: {python_path}")
            bt.logging.debug(f"Using runner at: {runner_path}")
            bt.logging.debug(f"Model path: {model_path}")
            bt.logging.debug(f"Input path: {input_path}")
            bt.logging.debug(f"Output path: {output_path}")

            process = subprocess.Popen(
                [python_path, runner_path, model_path, input_path, output_path],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

            stdout, stderr = process.communicate()

            if process.returncode != 0:
                bt.logging.error(f"ONNX runner failed with code {process.returncode}")
                bt.logging.error(f"STDOUT: {stdout}")
                bt.logging.error(f"STDERR: {stderr}")
                return None

            try:
                return np.load(output_path)
            except Exception as e:
                bt.logging.error(f"Failed to load ONNX output: {e}")
                return None

        except Exception as e:
            bt.logging.error(f"Error running ONNX model: {e}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return None
