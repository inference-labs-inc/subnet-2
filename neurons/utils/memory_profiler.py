import gc
import linecache
import os
import sys
import threading
import time
import tracemalloc

import psutil

PROFILE_INTERVAL = 120
TOP_N = 15
SNAPSHOT_COMPARE_INTERVAL = 600

_prev_snapshot = None
_prev_snapshot_time = 0
_process = psutil.Process(os.getpid())
_started = False


def _get_rss_mb():
    return _process.memory_info().rss / (1024 * 1024)


def _get_fd_count():
    try:
        return _process.num_fds()
    except Exception:
        return -1


def _get_thread_count():
    return threading.active_count()


def _get_tcp_count():
    try:
        connections = _process.net_connections(kind="tcp")
        states = {}
        for conn in connections:
            state = conn.status
            states[state] = states.get(state, 0) + 1
        return states
    except Exception:
        return {}


def _sizeof_fmt(num):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if abs(num) < 1024.0:
            return f"{num:.1f} {unit}"
        num /= 1024.0
    return f"{num:.1f} TiB"


def _format_tracemalloc_top(snapshot, limit=TOP_N):
    stats = snapshot.statistics("lineno")
    lines = []
    for idx, stat in enumerate(stats[:limit], 1):
        frame = stat.traceback[0]
        lines.append(
            f"  #{idx}: {frame.filename}:{frame.lineno} "
            f"size={_sizeof_fmt(stat.size)} count={stat.count}"
        )
        line = linecache.getline(frame.filename, frame.lineno).strip()
        if line:
            lines.append(f"         {line}")
    total = sum(s.size for s in stats)
    lines.append(f"  Total tracked: {_sizeof_fmt(total)} in {len(stats)} locations")
    return "\n".join(lines)


def _format_snapshot_diff(old_snapshot, new_snapshot, limit=TOP_N):
    stats = new_snapshot.compare_to(old_snapshot, "lineno")
    lines = []
    growing = [s for s in stats if s.size_diff > 0]
    growing.sort(key=lambda s: s.size_diff, reverse=True)
    for idx, stat in enumerate(growing[:limit], 1):
        frame = stat.traceback[0]
        lines.append(
            f"  #{idx}: {frame.filename}:{frame.lineno} "
            f"+{_sizeof_fmt(stat.size_diff)} (now {_sizeof_fmt(stat.size)}) "
            f"count_diff={stat.count_diff:+d}"
        )
        line = linecache.getline(frame.filename, frame.lineno).strip()
        if line:
            lines.append(f"         {line}")
    return "\n".join(lines)


def _inspect_validator_objects():
    lines = []
    try:
        for obj in gc.get_objects():
            obj_type = type(obj).__name__
            if obj_type == "ValidatorLoop":
                lines.append(f"  ValidatorLoop.active_tasks: {len(obj.active_tasks)}")
                lines.append(
                    f"  ValidatorLoop.recent_responses: {len(obj.recent_responses)}"
                )
                lines.append(
                    f"  ValidatorLoop.miner_active_count entries: {len(obj.miner_active_count)}"
                )
                lines.append(
                    f"  ValidatorLoop.request_queue size: {obj.request_queue.qsize()}"
                )
                if hasattr(obj, "relay"):
                    lines.append(
                        f"  Relay.stacked_requests_queue: {obj.relay.stacked_requests_queue.qsize()}"
                    )
                    lines.append(f"  Relay.rwr_queue: {obj.relay.rwr_queue.qsize()}")
                    lines.append(
                        f"  Relay.pending_requests: {len(obj.relay.pending_requests)}"
                    )
                    lines.append(
                        f"  Relay.request_results: {len(obj.relay.request_results)}"
                    )
                    lines.append(
                        f"  Relay._pending_notifications: {len(obj.relay._pending_notifications)}"
                    )
                if hasattr(obj, "score_manager"):
                    pm = obj.score_manager.pow_manager
                    lines.append(
                        f"  PoW queue length: {len(pm.proof_of_weights_queue)}"
                    )
                    queue_size_bytes = sys.getsizeof(pm.proof_of_weights_queue)
                    lines.append(
                        f"  PoW queue list object size: {_sizeof_fmt(queue_size_bytes)}"
                    )
                if hasattr(obj, "dsperse_manager"):
                    dm = obj.dsperse_manager
                    lines.append(f"  DSperseManager.runs: {len(dm.runs)}")
                    lines.append(
                        f"  DSperseManager._incremental_runs: {len(dm._incremental_runs)}"
                    )
                    if dm._incremental_runner:
                        ir = dm._incremental_runner
                        lines.append(f"  IncrementalRunner._runs: {len(ir._runs)}")
                        for rid, rs in ir._runs.items():
                            tc_size = sum(
                                t.nelement() * t.element_size()
                                for t in rs.tensor_cache.values()
                                if hasattr(t, "nelement")
                            )
                            lines.append(
                                f"    Run {rid}: tensor_cache={len(rs.tensor_cache)} entries "
                                f"({_sizeof_fmt(tc_size)}), "
                                f"pending={len(rs.pending_work)}, "
                                f"completed={len(rs.completed_slices)}, "
                                f"idx={rs.current_idx}/{len(rs.execution_order)}"
                            )
                if hasattr(obj, "dsperse_event_client"):
                    lines.append(
                        f"  DsperseEventClient._buffer: {len(obj.dsperse_event_client._buffer)}"
                    )
                break
    except Exception as e:
        lines.append(f"  Error inspecting objects: {e}")
    return "\n".join(lines)


def _type_census():
    gc.collect()
    type_counts = {}
    type_sizes = {}
    for obj in gc.get_objects():
        t = type(obj).__name__
        type_counts[t] = type_counts.get(t, 0) + 1
        try:
            type_sizes[t] = type_sizes.get(t, 0) + sys.getsizeof(obj)
        except (TypeError, OverflowError):
            continue
    by_count = sorted(type_counts.items(), key=lambda x: x[1], reverse=True)[:20]
    by_size = sorted(type_sizes.items(), key=lambda x: x[1], reverse=True)[:20]
    lines = ["  Top by count:"]
    for name, count in by_count:
        lines.append(f"    {name}: {count}")
    lines.append("  Top by size:")
    for name, size in by_size:
        lines.append(f"    {name}: {_sizeof_fmt(size)}")
    return "\n".join(lines)


def _profiler_loop():
    global _prev_snapshot, _prev_snapshot_time
    import bittensor as bt

    bt.logging.warning(
        f"[MEMPROF] Memory profiler started (interval={PROFILE_INTERVAL}s)"
    )

    time.sleep(30)

    _prev_snapshot = tracemalloc.take_snapshot()
    _prev_snapshot_time = time.time()

    while True:
        try:
            time.sleep(PROFILE_INTERVAL)

            rss = _get_rss_mb()
            fds = _get_fd_count()
            threads = _get_thread_count()
            tcp_states = _get_tcp_count()

            bt.logging.warning(
                f"[MEMPROF] RSS={rss:.0f}MB | FDs={fds} | Threads={threads} | "
                f"TCP={tcp_states}"
            )

            snapshot = tracemalloc.take_snapshot()
            snapshot = snapshot.filter_traces(
                [
                    tracemalloc.Filter(False, "<frozen *>"),
                    tracemalloc.Filter(False, "<unknown>"),
                    tracemalloc.Filter(False, tracemalloc.__file__),
                ]
            )

            bt.logging.warning(
                f"[MEMPROF] Top allocations:\n{_format_tracemalloc_top(snapshot)}"
            )

            elapsed_since_prev = time.time() - _prev_snapshot_time
            if (
                elapsed_since_prev >= SNAPSHOT_COMPARE_INTERVAL
                and _prev_snapshot is not None
            ):
                diff = _format_snapshot_diff(_prev_snapshot, snapshot)
                bt.logging.warning(
                    f"[MEMPROF] Growth since {elapsed_since_prev:.0f}s ago:\n{diff}"
                )
                _prev_snapshot = snapshot
                _prev_snapshot_time = time.time()

            bt.logging.warning(
                f"[MEMPROF] Validator internals:\n{_inspect_validator_objects()}"
            )

            if rss > 5000:
                bt.logging.warning(
                    f"[MEMPROF] Type census (RSS > 5GB):\n{_type_census()}"
                )

        except Exception as e:
            bt.logging.error(f"[MEMPROF] Error: {e}")


def start():
    global _started
    if _started:
        return
    _started = True
    if not tracemalloc.is_tracing():
        tracemalloc.start(10)
    t = threading.Thread(target=_profiler_loop, daemon=True, name="memory-profiler")
    t.start()
