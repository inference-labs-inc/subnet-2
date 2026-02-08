from __future__ import annotations

import asyncio
import concurrent.futures
import os
import random
import sys
import time
import traceback
from typing import NoReturn

import bittensor as bt
import cli_parser
import httpx
from bittensor.core.chain_data import AxonInfo
from deployment_layer.circuit_store import circuit_store
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.dsperse_event_client import DsperseEventClient

from _validator.api import RelayManager
from _validator.api.client import query_miner
from _validator.config import ValidatorConfig
from _validator.core.capacity_manager import CapacityManager
from _validator.core.exceptions import EmptyProofException, IncorrectProofException
from _validator.core.prometheus import (
    log_error,
    log_queue_metrics,
    log_score_change,
    log_system_metrics,
    log_weight_update,
    start_prometheus_logging,
    stop_prometheus_logging,
)
from _validator.core.request import Request
from _validator.core.request_pipeline import RequestPipeline
from _validator.core.response_processor import ResponseProcessor
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType
from _validator.pow.proof_of_weights_handler import ProofOfWeightsHandler
from _validator.scoring.score_manager import ScoreManager
from _validator.scoring.weights import WeightsManager
from _validator.utils.logging import log_responses as console_log_responses
from _validator.utils.proof_of_weights import save_proof_of_weights
from _validator.utils.uid import get_queryable_uids
from constants import (
    BATCHED_PROOF_OF_WEIGHTS_MODEL_ID,
    DEFAULT_PROOF_SIZE,
    EXCEPTION_DELAY_SECONDS,
    FIVE_MINUTES,
    LOOP_DELAY_SECONDS,
    MAX_CONCURRENT_REQUESTS,
    ONE_HOUR,
    ONE_MINUTE,
    TEN_MINUTES,
)
from utils import AutoUpdate, clean_temp_files, with_rate_limit
from utils.gc_logging import log_responses as gc_log_responses

# Set to True for synchronous request processing (easier debugging)
DEBUG_SYNC_MODE = os.environ.get("DEBUG_SYNC_MODE", "").lower() in ("1", "true", "yes")

MAX_SLICE_RETRIES = 3


class ValidatorLoop:
    """
    Main loop for the validator node.

    The main loop for the validator. Handles everything from score updates to weight updates.
    """

    def __init__(self, config: ValidatorConfig):
        """
        Initialize the ValidatorLoop based on provided configuration.

        Args:
            config (bt.Config): Bittensor configuration object.
        """
        self.config = config
        self.config.check_register()
        self.auto_update = AutoUpdate()
        self.httpx_client = httpx.AsyncClient()

        self.current_concurrency = MAX_CONCURRENT_REQUESTS
        self.capacity_manager = CapacityManager(self.config, self.httpx_client)

        api_url = (
            getattr(cli_parser.config, "sn2_api_url", None)
            or "https://sn2-api.inferencelabs.com"
        )
        self.dsperse_event_client = DsperseEventClient(api_url, config.wallet)

        self.dsperse_manager = DSperseManager(
            event_client=self.dsperse_event_client,
            incremental_mode=True,
        )

        self.score_manager = ScoreManager(
            self.config.metagraph,
            self.config.user_uid,
            self.config.full_path_score,
        )
        self.response_processor = ResponseProcessor(self.dsperse_manager)
        self.weights_manager = WeightsManager(
            self.config.subtensor,
            self.config.metagraph,
            self.config.wallet,
            self.config.user_uid,
            score_manager=self.score_manager,
        )
        self.last_pow_commit_block = 0
        self.relay = RelayManager(self.config)
        self.relay.dsperse_manager = self.dsperse_manager
        self.request_pipeline = RequestPipeline(
            self.config, self.score_manager, self.relay
        )

        self.request_queue = asyncio.Queue()
        self.active_tasks: dict[str, asyncio.Task | None] = {}
        self.miner_active_count: dict[int, int] = {}
        self.miner_capacities: dict[int, int] = {}
        self.default_miner_capacity = 10
        self.queryable_uids: list[int] = []
        self.last_response_time = time.time()
        self.last_periodic_task_time = time.time()
        self._task_counter = 0

        self._should_run = True

        self.thread_pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)
        self.response_thread_pool = concurrent.futures.ThreadPoolExecutor(
            max_workers=16
        )
        self.recent_responses: list[MinerResponse] = []

        if self.config.bt_config.prometheus_monitoring:
            start_prometheus_logging(self.config.bt_config.prometheus_port)

    @with_rate_limit(period=ONE_MINUTE)
    async def update_weights(self):
        start_time = time.time()
        try:
            # Run blocking blockchain call in thread pool with timeout
            await asyncio.wait_for(
                asyncio.get_event_loop().run_in_executor(
                    self.thread_pool,
                    self.weights_manager.update_weights,
                    self.score_manager.scores,
                ),
                timeout=120.0,  # 2 minute timeout
            )
            duration = time.time() - start_time
            log_weight_update(duration, success=True)
        except asyncio.TimeoutError:
            bt.logging.error("Weight update timed out after 120 seconds")
            log_weight_update(120.0, success=False, failure_reason="timeout")
        except Exception as e:
            log_weight_update(0.0, success=False, failure_reason=str(e))
            log_error("weight_update", "weights_manager", str(e))
            raise

    @with_rate_limit(period=ONE_HOUR)
    async def sync_scores_uids(self):
        try:
            await asyncio.wait_for(
                asyncio.get_event_loop().run_in_executor(
                    self.thread_pool,
                    self.score_manager.sync_scores_uids,
                    self.config.metagraph.uids.tolist(),
                ),
                timeout=60.0,  # 1 minute timeout
            )
        except asyncio.TimeoutError:
            bt.logging.error("sync_scores_uids timed out after 60 seconds")
        except Exception as e:
            bt.logging.error(f"Error in sync_scores_uids: {e}")

    @with_rate_limit(period=ONE_HOUR)
    async def sync_metagraph(self):
        try:
            await asyncio.wait_for(
                asyncio.get_event_loop().run_in_executor(
                    self.thread_pool,
                    lambda: self.config.metagraph.sync(subtensor=self.config.subtensor),
                ),
                timeout=120.0,  # 2 minute timeout
            )
        except asyncio.TimeoutError:
            bt.logging.error("sync_metagraph timed out after 120 seconds")
        except Exception as e:
            bt.logging.error(f"Error in sync_metagraph: {e}")

    @with_rate_limit(period=ONE_HOUR)
    async def sync_capacities(self, miners_info: dict[int, AxonInfo]):
        capacities_by_uid = await self.capacity_manager.sync_capacities(miners_info)
        bt.logging.debug(f"Synced capacities for {len(capacities_by_uid)} miners")

        for uid in miners_info:
            response = capacities_by_uid.get(uid)
            if response and hasattr(response, "capacities") and response.capacities:
                total_capacity = sum(response.capacities.values())
                self.miner_capacities[uid] = (
                    min(total_capacity, 20)
                    if total_capacity > 0
                    else self.default_miner_capacity
                )
            else:
                self.miner_capacities[uid] = self.default_miner_capacity

        bt.logging.info(f"Updated capacities for {len(self.miner_capacities)} miners")
        return capacities_by_uid

    @with_rate_limit(period=FIVE_MINUTES)
    def check_auto_update(self):
        self._handle_auto_update()

    @with_rate_limit(period=TEN_MINUTES)
    async def refresh_circuits(self):
        await asyncio.get_event_loop().run_in_executor(
            self.thread_pool, circuit_store.refresh_circuits
        )

    @with_rate_limit(period=FIVE_MINUTES)
    def update_queryable_uids(self):
        self.queryable_uids = list(get_queryable_uids(self.config.metagraph))

    @with_rate_limit(period=ONE_MINUTE / 4)
    def log_health(self):
        bt.logging.info(
            f"In-flight requests: {len(self.active_tasks)} / {MAX_CONCURRENT_REQUESTS}"
        )
        bt.logging.debug(f"Queryable UIDs: {len(self.queryable_uids)}")

        log_system_metrics()
        queue_size = self.request_queue.qsize()
        est_latency = (
            queue_size * (LOOP_DELAY_SECONDS / MAX_CONCURRENT_REQUESTS)
            if queue_size > 0
            else 0
        )
        log_queue_metrics(queue_size, est_latency)

    @with_rate_limit(period=ONE_MINUTE)
    async def log_responses(self):
        if self.recent_responses:
            console_log_responses(self.recent_responses)

            if not cli_parser.config.disable_metric_logging:
                try:
                    block = (
                        self.config.metagraph.block.item()
                        if self.config.metagraph.block is not None
                        else 0
                    )
                    _ = await asyncio.get_event_loop().run_in_executor(
                        self.thread_pool,
                        lambda: gc_log_responses(
                            self.config.metagraph,
                            self.config.wallet.hotkey,
                            self.config.user_uid,
                            self.recent_responses,
                            (
                                time.time() - self.last_response_time
                                if hasattr(self, "last_response_time")
                                else 0
                            ),
                            block,
                            self.score_manager.scores,
                        ),
                    )
                except Exception as e:
                    bt.logging.error(f"Error in GC logging: {e}")

            self.last_response_time = time.time()
            self.recent_responses = []

    async def maintain_request_pool(self):
        """
        Maintain the pool of active requests to miners.
        Supports multiple concurrent requests per miner based on their capacity.
        """
        while self._should_run:
            try:
                slots_available = self.current_concurrency - len(self.active_tasks)

                if not slots_available:
                    await asyncio.sleep(1)
                    continue

                if (
                    self.relay.stacked_requests_queue.empty()
                    and not self.dsperse_manager.has_work_in_flight()
                ):
                    new_requests = list(self.dsperse_manager.generate_dslice_requests())
                    if new_requests:
                        bt.logging.info(
                            f"Generated {len(new_requests)} new requests, inserting into queue"
                        )
                    for dslice_request in new_requests:
                        self.relay.stacked_requests_queue.put_nowait(dslice_request)

                pow_circuit = None
                if (
                    len(self.score_manager.pow_manager.proof_of_weights_queue)
                    >= ProofOfWeightsHandler.BATCH_SIZE
                ):
                    loop = asyncio.get_event_loop()
                    pow_circuit = await loop.run_in_executor(
                        self.thread_pool,
                        circuit_store.ensure_circuit,
                        BATCHED_PROOF_OF_WEIGHTS_MODEL_ID,
                    )

                requests_sent = 0
                shuffled_uids = self.queryable_uids.copy()
                random.shuffle(shuffled_uids)
                for uid in shuffled_uids:
                    if requests_sent >= slots_available:
                        break

                    miner_capacity = self.miner_capacities.get(
                        uid, self.default_miner_capacity
                    )
                    miner_active = self.miner_active_count.get(uid, 0)
                    available_slots = miner_capacity - miner_active

                    if available_slots <= 0:
                        continue

                    requests_for_miner = min(
                        available_slots, slots_available - requests_sent
                    )

                    for _ in range(requests_for_miner):
                        if pow_circuit is not None:
                            request = self.request_pipeline._prepare_benchmark_request(
                                uid,
                                pow_circuit,
                            )
                        elif not self.relay.stacked_requests_queue.empty():
                            request = self.request_pipeline._prepare_queued_request(uid)
                        else:
                            request = self.request_pipeline._prepare_benchmark_request(
                                uid
                            )

                        if not request:
                            bt.logging.warning(
                                f"Empty request prepared for UID {uid}, skipping"
                            )
                            break

                        task_id = self._generate_task_id(uid)

                        if DEBUG_SYNC_MODE:
                            bt.logging.debug(
                                f"[SYNC MODE] Processing request {task_id} for UID {uid}"
                            )
                            self.active_tasks[task_id] = None
                            self.miner_active_count[uid] = (
                                self.miner_active_count.get(uid, 0) + 1
                            )
                            await self._process_single_request(request)
                            self._handle_completed_task(task_id, uid)
                        else:
                            task = asyncio.create_task(
                                self._process_single_request(request)
                            )
                            self.active_tasks[task_id] = task
                            self.miner_active_count[uid] = (
                                self.miner_active_count.get(uid, 0) + 1
                            )
                            task.add_done_callback(
                                lambda _, tid=task_id, u=uid: self._handle_completed_task(
                                    tid, u
                                )
                            )

                        requests_sent += 1

                await asyncio.sleep(0)
            except Exception as e:
                bt.logging.error(f"Error maintaining request pool: {e}")
                traceback.print_exc()
                await asyncio.sleep(EXCEPTION_DELAY_SECONDS)

    def _generate_task_id(self, uid: int) -> str:
        """Generate a unique task ID for tracking."""
        self._task_counter += 1
        return f"{uid}_{self._task_counter}_{time.time()}"

    def _handle_completed_task(self, task_id: str, uid: int):
        if task_id in self.active_tasks:
            del self.active_tasks[task_id]

        if uid in self.miner_active_count:
            self.miner_active_count[uid] = max(0, self.miner_active_count[uid] - 1)

    async def run_periodic_tasks(self):
        while self._should_run:
            try:

                self.check_auto_update()
                await self.refresh_circuits()
                await self.sync_metagraph()
                await self.sync_scores_uids()
                await self.update_weights()
                self.update_queryable_uids()
                await self.sync_capacities(
                    {
                        uid: self.config.metagraph.axons[uid]
                        for uid in self.queryable_uids
                    }
                )
                self.log_health()
                await self.log_responses()
                self.last_periodic_task_time = time.time()
                await asyncio.sleep(LOOP_DELAY_SECONDS)
            except Exception as e:
                bt.logging.error(f"Error in periodic tasks: {e}")
                traceback.print_exc()
                await asyncio.sleep(EXCEPTION_DELAY_SECONDS)

    async def watchdog(self):
        """
        Monitor for validator freezes and log diagnostic information.

        This coroutine periodically checks if the validator is making progress
        and logs warnings if no activity is detected for extended periods.
        """
        WATCHDOG_INTERVAL = 60  # Check every minute
        INACTIVITY_THRESHOLD = 300  # Warn after 5 minutes of no activity

        while self._should_run:
            try:
                await asyncio.sleep(WATCHDOG_INTERVAL)

                time_since_last_response = time.time() - self.last_response_time
                time_since_last_periodic = time.time() - self.last_periodic_task_time
                problem_detected = False

                # Check for periodic tasks freeze (more critical - indicates blocking)
                if time_since_last_periodic > INACTIVITY_THRESHOLD:
                    bt.logging.error(
                        f"WATCHDOG: Periodic tasks stalled for {time_since_last_periodic:.0f}s! "
                        f"Possible blocking operation in progress."
                    )
                    problem_detected = True

                # Check for response inactivity (less critical - could be network issues)
                if time_since_last_response > INACTIVITY_THRESHOLD:
                    bt.logging.warning(
                        f"WATCHDOG: No miner responses for {time_since_last_response:.0f}s."
                    )
                    problem_detected = True

                if problem_detected:
                    # Log thread pool status to help diagnose
                    try:
                        thread_pool_queued = self.thread_pool._work_queue.qsize()
                        response_pool_queued = (
                            self.response_thread_pool._work_queue.qsize()
                        )
                        bt.logging.warning(
                            f"WATCHDOG: Thread pools - main: {thread_pool_queued} queued, "
                            f"response: {response_pool_queued} queued"
                        )
                    except Exception:
                        pass  # Thread pool internals may not be accessible
                    bt.logging.warning(
                        f"Active tasks: {len(self.active_tasks)}/{MAX_CONCURRENT_REQUESTS}, "
                        f"Queue size: {self.request_queue.qsize()}, "
                        f"Queryable UIDs: {len(self.queryable_uids)}, "
                        f"Current concurrency: {self.current_concurrency}"
                    )
                else:
                    bt.logging.debug(
                        f"WATCHDOG: Healthy - last response {time_since_last_response:.0f}s ago, "
                        f"last periodic {time_since_last_periodic:.0f}s ago"
                    )
            except asyncio.CancelledError:
                break
            except Exception as e:
                bt.logging.error(f"WATCHDOG error: {e}")

    async def run(self) -> NoReturn:
        """
        Run the main validator loop indefinitely.
        """
        bt.logging.success(
            f"Validator started on subnet {self.config.subnet_uid} using UID {self.config.user_uid}"
        )

        # Start the relay client connection
        self.relay.start()
        self.dsperse_event_client.start()

        try:
            await asyncio.gather(
                self.maintain_request_pool(),
                self.run_periodic_tasks(),
                self.watchdog(),
            )
        except KeyboardInterrupt:
            self._should_run = False
            bt.logging.success("Keyboard interrupt detected. Exiting validator.")
        except Exception as e:
            bt.logging.error(f"Fatal error in validator loop: {e}")
            raise
        finally:
            await self._cleanup()

    async def _process_single_request(self, request: Request) -> None:
        """
        Perform a single request to a miner and handle the response.
        """
        response: MinerResponse | None = None
        try:
            response = await query_miner(
                self.httpx_client,
                request,
                self.config.wallet,
            )

            if DEBUG_SYNC_MODE:
                # Direct sync call for easier debugging
                response = self.response_processor.verify_single_response(
                    request, response
                )
            else:
                # Run in thread pool to avoid blocking event loop
                response: (
                    MinerResponse | None
                ) = await asyncio.get_event_loop().run_in_executor(
                    self.response_thread_pool,
                    self.response_processor.verify_single_response,
                    request,
                    response,
                )

        except (EmptyProofException, IncorrectProofException) as e:
            bt.logging.warning(f"{e.message}")
            self._reschedule_request(request)
        except httpx.InvalidURL:
            bt.logging.warning(
                f"Ignoring UID as there is not a valid URL: {request.uid}. {request.ip}:{request.port}"
            )
            self._reschedule_request(request)
        except httpx.HTTPError as e:
            bt.logging.warning(
                f"Failed to query miner for UID: {request.uid}. {request.ip}:{request.port} Error: {e}"
            )
            self._reschedule_request(request)
        except Exception as e:
            bt.logging.error(f"Error processing request for UID {request.uid}: {e}")
            traceback.print_exc()
            log_error("request_processing", "axon_query", str(e))
            self._reschedule_request(request)
        finally:
            if response:
                await self._handle_response(response)

    def _reschedule_request(self, request: Request) -> None:
        """
        Reschedule a failed request for retry.
        Only RWR and DSLICE requests are rescheduled, up to MAX_SLICE_RETRIES times.
        """
        if request.request_type not in (RequestType.RWR, RequestType.DSLICE):
            bt.logging.debug(
                f"Not rescheduling request type {request.request_type} for UID {request.uid}"
            )
            return

        if not request.queued_request:
            bt.logging.debug(
                f"No queued request found for rescheduling for UID {request.uid}"
            )
            return

        queued = request.queued_request
        queued.retry_count += 1

        if queued.retry_count > MAX_SLICE_RETRIES:
            bt.logging.warning(
                f"{request.request_type.name} request exceeded max retries "
                f"({MAX_SLICE_RETRIES}) for UID {request.uid}"
            )
            self.request_pipeline.hash_guard.remove_hash(request.guard_hash)
            if request.request_type == RequestType.DSLICE:
                self._mark_dslice_failed(queued)
            elif request.request_type == RequestType.RWR:
                self.relay.set_request_result(
                    request.external_request_hash,
                    {"success": False, "error": "Max retries exceeded"},
                )
            return

        bt.logging.info(
            f"Rescheduling {request.request_type.name} request for UID {request.uid} "
            f"(attempt {queued.retry_count}/{MAX_SLICE_RETRIES})..."
        )

        self.request_pipeline.hash_guard.remove_hash(request.guard_hash)
        self.relay.stacked_requests_queue.put_nowait(queued)

    def _mark_dslice_failed(self, queued: DSliceQueuedProofRequest) -> None:
        if getattr(queued, "is_tile", False):
            parts = str(queued.slice_num).split("_tile_")
            base_slice = parts[0]
            tile_idx = int(parts[1])
            slice_id = f"slice_{base_slice}"
            task_id = getattr(queued, "task_id", f"{slice_id}_tile_{tile_idx}")

            self.dsperse_manager.on_incremental_tile_result(
                run_uid=queued.run_uid,
                task_id=task_id,
                slice_id=slice_id,
                tile_idx=tile_idx,
                success=False,
            )
        elif self.dsperse_manager.is_incremental_run(queued.run_uid):
            self.dsperse_manager.on_incremental_slice_result(
                run_uid=queued.run_uid,
                slice_num=str(queued.slice_num),
                success=False,
            )
        else:
            self.dsperse_manager.on_slice_result(
                queued.run_uid, str(queued.slice_num), success=False
            )

    def _mark_dslice_complete(self, response: MinerResponse) -> None:
        run_uid = response.dsperse_run_uid
        slice_num = response.dsperse_slice_num
        if not run_uid or slice_num is None:
            bt.logging.warning(
                f"Cannot mark DSLICE complete: missing run_uid={run_uid} or slice_num={slice_num}"
            )
            return

        is_tile = "_tile_" in str(slice_num)

        if is_tile:
            parts = str(slice_num).split("_tile_")
            base_slice = parts[0]
            tile_idx = int(parts[1])
            slice_id = f"slice_{base_slice}"
            task_id = f"{slice_id}_tile_{tile_idx}"

            is_complete, next_requests = (
                self.dsperse_manager.on_incremental_tile_result(
                    run_uid=run_uid,
                    task_id=task_id,
                    slice_id=slice_id,
                    tile_idx=tile_idx,
                    success=True,
                    computed_outputs=response.computed_outputs,
                    proof=response.proof_content,
                    witness=response.witness,
                    response_time_sec=response.response_time,
                    verification_time_sec=response.verification_time or 0.0,
                )
            )
            if next_requests and not is_complete:
                queue = self.relay.stacked_requests_queue
                bt.logging.info(
                    f"Inserting {len(next_requests)} items into queue (size: {queue.qsize()})"
                )
                for req in next_requests:
                    queue.put_nowait(req)
                bt.logging.info(
                    f"Queued {len(next_requests)} items for {run_uid} (size now: {queue.qsize()})"
                )
        elif response.is_incremental or self.dsperse_manager.is_incremental_run(
            run_uid
        ):
            is_complete, next_request = (
                self.dsperse_manager.on_incremental_slice_result(
                    run_uid=run_uid,
                    slice_num=str(slice_num),
                    success=True,
                    computed_outputs=response.computed_outputs,
                    proof=response.proof_content,
                    response_time_sec=response.response_time,
                    verification_time_sec=response.verification_time or 0.0,
                )
            )
            if next_request and not is_complete:
                self.relay.stacked_requests_queue.put_nowait(next_request)
                bt.logging.debug(f"Queued next incremental slice for run {run_uid}")
        else:
            self.dsperse_manager.on_slice_result(
                run_uid,
                str(slice_num),
                success=True,
                response_time_sec=response.response_time,
                verification_time_sec=response.verification_time or 0.0,
            )

    async def _handle_response(self, response: MinerResponse) -> None:
        """
        Handle a processed response, updating scores and weights as needed.

        Args:
            response (MinerResponse): The processed response to handle.
        """
        try:
            request_hash = response.external_request_hash
            if response.verification_result:
                bt.logging.info(
                    f"Successfully verified proof from UID {response.uid} "
                    f"for circuit {response.circuit.metadata.name} ({response.circuit.metadata.version}), "
                    f"using {response.proof_system}. "
                    f"Request type: {response.request_type.name}"
                )
            else:
                response.response_time = (
                    response.circuit.evaluation_data.maximum_response_time
                )
                response.proof_size = DEFAULT_PROOF_SIZE
            self.recent_responses.append(response)
            if response.request_type == RequestType.RWR:
                if response.verification_result:
                    self.relay.set_request_result(
                        request_hash,
                        {
                            "hash": request_hash,
                            "public_signals": response.public_json,
                            "proof": response.proof_content,
                            "success": True,
                        },
                    )
                else:
                    self.relay.set_request_result(
                        request_hash,
                        {
                            "success": False,
                        },
                    )
            elif (
                response.request_type == RequestType.DSLICE
                and response.verification_result
            ):
                self._mark_dslice_complete(response)

            if response.verification_result and response.save:
                save_proof_of_weights(
                    public_signals=[response.public_json],
                    proof=[response.proof_content],
                    metadata={
                        "circuit": str(response.circuit),
                        "request_hash": request_hash,
                        "miner_uid": response.uid,
                    },
                    hotkey=self.config.wallet.hotkey,
                    is_testnet=self.config.subnet_uid == 118,
                    proof_filename=request_hash,
                )

            old_score = self.score_manager._get_safe_score(response.uid)
            self.score_manager.update_single_score(response, self.queryable_uids)
            new_score = self.score_manager._get_safe_score(response.uid)
            log_score_change(old_score, new_score)

        except Exception as e:
            bt.logging.error(f"Error handling response: {e}")
            traceback.print_exc()
            log_error("response_handling", "response_processor", str(e))

    def _handle_auto_update(self):
        """Handle automatic updates if enabled."""
        if not self.config.bt_config.no_auto_update:
            self.auto_update.try_update()
        else:
            bt.logging.debug("Automatic updates are disabled, skipping version check")

    async def _cleanup(self):
        """Handle keyboard interrupt by cleaning up and exiting."""
        bt.logging.success("Keyboard interrupt detected. Exiting validator.")
        await self.relay.stop()
        await self.httpx_client.aclose()
        stop_prometheus_logging()
        clean_temp_files()
        self.dsperse_manager.total_cleanup()
        sys.exit(0)
