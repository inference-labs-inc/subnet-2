from __future__ import annotations

import asyncio
import concurrent.futures
import os
import sys
import time
import traceback
from multiprocessing import Queue as MPQueue
from queue import Empty
from typing import NoReturn

import bittensor as bt
import httpx
from bittensor.core.chain_data import AxonInfo

from _validator.api import ValidatorAPI
from _validator.api.client import query_miner
from _validator.competitions.competition import Competition
from _validator.config import ValidatorConfig
from _validator.core.capacity_manager import CapacityManager
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
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType, ValidatorMessage
from _validator.scoring.score_manager import ScoreManager
from _validator.scoring.weights import WeightsManager
from _validator.utils.logging import log_responses as console_log_responses
from _validator.utils.proof_of_weights import save_proof_of_weights
from _validator.utils.uid import get_queryable_uids
from constants import (
    DEFAULT_PROOF_SIZE,
    EXCEPTION_DELAY_SECONDS,
    FIVE_MINUTES,
    LOOP_DELAY_SECONDS,
    MAX_CONCURRENT_REQUESTS,
    ONE_HOUR,
    ONE_MINUTE,
)
from utils import AutoUpdate, clean_temp_files, with_rate_limit
from utils.gc_logging import gc_log_competition_metrics
from utils.gc_logging import log_responses as gc_log_responses


class ValidatorLoop:
    """
    Main loop for the validator node.

    The main loop for the validator. Handles everything from score updates to weight updates.
    """

    def __init__(self, config: ValidatorConfig):
        """
        Initialize the ValidatorLoop based on provided configuration.

        Args:
            config (bt.config): Bittensor configuration object.
        """
        self.config = config
        self.config.check_register()
        self.auto_update = AutoUpdate()
        self.httpx_client = httpx.AsyncClient()

        self.validator_to_competition_queue = MPQueue()  # Messages TO competition
        self.competition_to_validator_queue = MPQueue()  # Messages FROM competition
        self.current_concurrency = MAX_CONCURRENT_REQUESTS
        self.capacity_manager = CapacityManager(self.config, self.httpx_client)
        try:
            competition_id = 1
            bt.logging.info("Initializing competition module...")
            self.competition = Competition(
                competition_id,
                self.config.metagraph,
                self.config.wallet,
                self.config.bt_config,
            )
            self.competition.set_validator_message_queues(
                self.validator_to_competition_queue, self.competition_to_validator_queue
            )
            bt.logging.success("Competition module initialized successfully")
            # self.clear_sota_state()
        except Exception as e:
            bt.logging.warning(
                f"Failed to initialize competition, continuing without competition support: {e}"
            )
            traceback.print_exc()
            self.competition = None

        self.score_manager = ScoreManager(
            self.config.metagraph,
            self.config.user_uid,
            self.config.full_path_score,
            self.competition,
        )
        self.response_processor = ResponseProcessor(
            self.config.metagraph,
            self.score_manager,
            self.config.user_uid,
            self.config.wallet.hotkey,
        )
        self.weights_manager = WeightsManager(
            self.config.subtensor,
            self.config.metagraph,
            self.config.wallet,
            self.config.user_uid,
            score_manager=self.score_manager,
        )
        self.last_pow_commit_block = 0
        self.api = ValidatorAPI(self.config)
        self.request_pipeline = RequestPipeline(
            self.config, self.score_manager, self.api
        )

        self.request_queue = asyncio.Queue()
        self.active_tasks: dict[int, asyncio.Task] = {}
        self.processed_uids: set[int] = set()
        self.queryable_uids: list[int] = []
        self.last_response_time = time.time()

        self._should_run = True

        self.thread_pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)
        self.response_thread_pool = concurrent.futures.ThreadPoolExecutor(
            max_workers=16
        )
        self.last_competition_sync = 0
        self.is_syncing_competition = False
        self.competition_commitments = []

        self.recent_responses: list[MinerResponse] = []

        if self.config.bt_config.prometheus_monitoring:
            start_prometheus_logging(self.config.bt_config.prometheus_port)

    def clear_sota_state(self):
        try:
            os.remove(
                os.path.join(
                    self.competition.competition_directory, "sota", "sota_state.json"
                )
            )
        except Exception as e:
            bt.logging.error(f"Error clearing sota state: {e}")
            traceback.print_exc()

    @with_rate_limit(period=ONE_MINUTE)
    def update_weights(self):
        start_time = time.time()
        try:
            self.weights_manager.update_weights(self.score_manager.scores)
            duration = time.time() - start_time
            log_weight_update(duration, success=True)
        except Exception as e:
            log_weight_update(0.0, success=False, failure_reason=str(e))
            log_error("weight_update", "weights_manager", str(e))
            raise

    @with_rate_limit(period=ONE_HOUR)
    def sync_scores_uids(self):
        self.score_manager.sync_scores_uids(self.config.metagraph.uids.tolist())

    @with_rate_limit(period=ONE_HOUR)
    def sync_metagraph(self):
        self.config.metagraph.sync(subtensor=self.config.subtensor)

    @with_rate_limit(period=ONE_HOUR)
    async def sync_capacities(self, axons: list[AxonInfo]):
        capacities = await self.capacity_manager.sync_capacities(axons)
        bt.logging.debug(f"Synced capacities: {capacities}")
        return capacities

    @with_rate_limit(period=FIVE_MINUTES)
    def check_auto_update(self):
        self._handle_auto_update()

    @with_rate_limit(period=FIVE_MINUTES)
    def update_queryable_uids(self):
        self.queryable_uids = list(get_queryable_uids(self.config.metagraph))

    @with_rate_limit(period=ONE_MINUTE / 4)
    def log_health(self):
        bt.logging.info(
            f"In-flight requests: {len(self.active_tasks)} / {MAX_CONCURRENT_REQUESTS}"
        )
        bt.logging.debug(f"Processed UIDs: {len(self.processed_uids)}")
        bt.logging.debug(f"Queryable UIDs: {len(self.queryable_uids)}")

        log_system_metrics()
        queue_size = self.request_queue.qsize()
        est_latency = (
            queue_size * (LOOP_DELAY_SECONDS / MAX_CONCURRENT_REQUESTS)
            if queue_size > 0
            else 0
        )
        log_queue_metrics(queue_size, est_latency)

    def update_processed_uids(self):
        if len(self.processed_uids) >= len(self.queryable_uids):
            self.processed_uids.clear()

    @with_rate_limit(period=ONE_HOUR)
    async def sync_competition(self):
        if not self.competition:
            bt.logging.debug("Competition module not initialized, skipping sync")
            return
        if self.is_syncing_competition:
            bt.logging.debug("Competition sync already in progress, skipping")
            return
        if not self.competition.is_active:
            bt.logging.debug("Competition is not active, skipping sync")
            return
        try:
            self.is_syncing_competition = True
            bt.logging.info("Starting competition sync...")
            bt.logging.debug("Fetching commitments...")
            commitments = self.competition.fetch_commitments()
            bt.logging.debug(f"Found {len(commitments)} commitments")
            if commitments:
                bt.logging.success(f"Found {len(commitments)} new circuits to evaluate")
                for uid, hotkey, hash in commitments:
                    if hash not in {
                        state.hash for state in self.competition.miner_states.values()
                    }:
                        bt.logging.debug(
                            f"Queueing download for circuit {hash[:8]}... from {hotkey[:8]}..."
                        )
                        self.competition.queue_download(uid, hotkey, hash)
                bt.logging.debug(
                    f"Queue size after adding: {len(self.competition.download_queue)}"
                )
            else:
                bt.logging.debug("No new circuits found during sync")
        except Exception as e:
            bt.logging.error(f"Error in competition sync: {e}")
            traceback.print_exc()
        finally:
            self.is_syncing_competition = False
            bt.logging.debug("Competition sync complete")

    @with_rate_limit(period=ONE_HOUR)
    def update_competition_metrics(self):
        bt.logging.debug("Updating competition metrics...")
        if self.competition and getattr(self.competition, "is_active", False):
            try:
                metrics_to_log = self.competition.get_summary_for_logging()

                if metrics_to_log:
                    metrics_to_log["validator_key"] = (
                        self.config.wallet.hotkey.ss58_address
                    )

                    bt.logging.debug("Logging competition metrics summary...")
                    response = gc_log_competition_metrics(
                        metrics_to_log, self.config.wallet.hotkey
                    )

                    if response and response.status_code == 200:
                        bt.logging.success(
                            "Successfully logged competition metrics summary."
                        )
                    else:
                        status_code = response.status_code if response else "N/A"
                        response_text = response.text if response else "N/A"
                        bt.logging.error(
                            "Failed to log competition metrics summary."
                            f" Status: {status_code}, Response: {response_text}"
                        )

            except Exception as e:
                bt.logging.error(
                    f"Error during competition metric summary logging: {e}",
                    exc_info=True,
                )

    @with_rate_limit(period=ONE_MINUTE)
    async def log_responses(self):
        if self.recent_responses:
            console_log_responses(self.recent_responses)

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
        while self._should_run:
            try:
                try:
                    message = await asyncio.get_event_loop().run_in_executor(
                        self.thread_pool,
                        lambda: self.competition_to_validator_queue.get(timeout=0.1),
                    )
                    if message == ValidatorMessage.WINDDOWN:
                        bt.logging.info(
                            "Received winddown message, reducing concurrency to zero"
                        )
                        self.current_concurrency = 0
                    elif message == ValidatorMessage.COMPETITION_COMPLETE:
                        bt.logging.info(
                            "Received competition complete message, restoring concurrency"
                        )
                        self.current_concurrency = MAX_CONCURRENT_REQUESTS
                except Empty:
                    bt.logging.trace("No messages in competition queue")
                except Exception as e:
                    bt.logging.error(f"Error in competition message handling: {e}")
                    traceback.print_exc()

                slots_available = self.current_concurrency - len(self.active_tasks)

                if slots_available > 0:
                    available_uids = [
                        uid
                        for uid in self.queryable_uids
                        if uid not in self.processed_uids
                        and uid not in self.active_tasks
                    ]

                    for uid in available_uids[:slots_available]:
                        request: Request = self.request_pipeline.prepare_single_request(
                            uid
                        )
                        if request:
                            task = asyncio.create_task(
                                self._process_single_request(request)
                            )
                            self.active_tasks[uid] = task
                            task.add_done_callback(
                                lambda t, uid=uid: self._handle_completed_task(t, uid)
                            )

                await asyncio.sleep(0)
            except Exception as e:
                bt.logging.error(f"Error maintaining request pool: {e}")
                traceback.print_exc()
                await asyncio.sleep(EXCEPTION_DELAY_SECONDS)

    def _handle_completed_task(self, task: asyncio.Task, uid: int):
        try:
            self.processed_uids.add(uid)
        except Exception as e:
            bt.logging.error(f"Error in task for UID {uid}: {e}")
            traceback.print_exc()
        finally:
            if uid in self.active_tasks:
                del self.active_tasks[uid]
                if (
                    self.current_concurrency == 0
                    and not self.active_tasks
                    and self.competition
                ):
                    bt.logging.info(
                        "All tasks completed during winddown, sending winddown complete message"
                    )
                    self.validator_to_competition_queue.put(
                        ValidatorMessage.WINDDOWN_COMPLETE
                    )

    async def run_periodic_tasks(self):
        while self._should_run:
            try:

                self.check_auto_update()
                self.sync_metagraph()
                self.sync_scores_uids()
                self.update_weights()
                self.update_competition_metrics()
                self.update_queryable_uids()
                await self.sync_capacities(
                    [self.config.metagraph.axons[uid] for uid in self.queryable_uids]
                )
                self.update_processed_uids()
                self.log_health()
                await self.log_responses()
                if self.current_concurrency:
                    await self.sync_competition()
                await asyncio.sleep(LOOP_DELAY_SECONDS)
            except Exception as e:
                bt.logging.error(f"Error in periodic tasks: {e}")
                traceback.print_exc()
                await asyncio.sleep(EXCEPTION_DELAY_SECONDS)

    async def run(self) -> NoReturn:
        """
        Run the main validator loop indefinitely.
        """
        bt.logging.success(
            f"Validator started on subnet {self.config.subnet_uid} using UID {self.config.user_uid}"
        )

        try:
            await asyncio.gather(
                self.maintain_request_pool(),
                self.run_periodic_tasks(),
            )
        except KeyboardInterrupt:
            self._should_run = False
            self._handle_keyboard_interrupt()
        except Exception as e:
            bt.logging.error(f"Fatal error in validator loop: {e}")
            raise

    async def _process_single_request(self, request: Request) -> Request:
        """
        Process a single request and return the response.
        """
        try:
            response = await query_miner(
                self.httpx_client,
                request,
                self.config.wallet,
            )
            if response is None:
                return request
            processed_response: (
                MinerResponse | None
            ) = await asyncio.get_event_loop().run_in_executor(
                self.response_thread_pool,
                self.response_processor.process_single_response,
                response,
            )
            if processed_response:
                await self._handle_response(processed_response)
        except Exception as e:
            bt.logging.error(f"Error processing request for UID {request.uid}: {e}")
            traceback.print_exc()
            log_error("request_processing", "axon_query", str(e))
        return request

    async def _handle_response(self, response: MinerResponse) -> None:
        """
        Handle a processed response, updating scores and weights as needed.

        Args:
            response (MinerResponse): The processed response to handle.
        """
        try:
            request_hash = response.input_hash
            if not response.verification_result:
                response.response_time = (
                    response.circuit.evaluation_data.maximum_response_time
                )
                response.proof_size = DEFAULT_PROOF_SIZE
            self.recent_responses.append(response)
            if response.request_type == RequestType.RWR:
                if response.verification_result:
                    self.api.set_request_result(
                        request_hash,
                        {
                            "hash": request_hash,
                            "public_signals": response.public_json,
                            "proof": response.proof_content,
                            "success": True,
                        },
                    )
                else:
                    self.api.set_request_result(
                        request_hash,
                        {
                            "success": False,
                        },
                    )

            if response.verification_result and response.save:
                save_proof_of_weights(
                    public_signals=[response.public_json],
                    proof=[response.proof_content],
                    metadata={
                        "circuit": str(response.circuit),
                        "request_hash": response.input_hash,
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

    def _handle_keyboard_interrupt(self):
        """Handle keyboard interrupt by cleaning up and exiting."""
        bt.logging.success("Keyboard interrupt detected. Exiting validator.")
        loop = asyncio.get_event_loop()
        loop.run_until_complete(self.api.stop())
        loop.run_until_complete(self.httpx_client.aclose())
        stop_prometheus_logging()
        clean_temp_files()
        if self.competition:
            self.competition.competition_thread.stop()
            if hasattr(self.competition.circuit_manager, "close"):
                loop.run_until_complete(self.competition.circuit_manager.close())
        sys.exit(0)
