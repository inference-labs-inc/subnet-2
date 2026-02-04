from __future__ import annotations

import statistics
import psutil
from wsgiref.simple_server import WSGIServer

from prometheus_client import Histogram, Gauge, Counter, start_http_server


class _PrometheusMetrics:
    def __init__(self, port: int):
        self.server: WSGIServer
        self.server, _ = start_http_server(port)

        self.validation_times = Histogram(
            "validating_seconds",
            "Time spent validating responses",
            buckets=(
                0.005,
                0.01,
                0.025,
                0.05,
                0.075,
                0.1,
                0.25,
                0.5,
                0.75,
                1.0,
                2.5,
                5.0,
                7.5,
                10.0,
            ),
        )
        self.response_times = Histogram(
            "requests_seconds",
            "Time spent processing requests",
            ["aggregation_type", "model"],
            buckets=(0.1, 0.5, 1.0, 2.5, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0),
        )
        self.proof_sizes = Histogram(
            "proof_sizes",
            "Size of proofs in bytes",
            ["aggregation_type", "model"],
            buckets=(1000, 2500, 5000, 7500, 10000, 25000, 50000, 75000, 100000),
        )
        self.verification_ratio = Histogram(
            "verified_proofs_ratio", "Ratio of successfully verified proofs", ["model"]
        )
        self.verification_failures = Counter(
            "verification_failures_total",
            "Total number of proof verification failures",
            ["model", "failure_type"],
        )
        self.timeout_counter = Counter(
            "timeouts_total", "Total number of request timeouts", ["model"]
        )
        self.network_errors = Counter(
            "network_errors_total", "Total number of network errors", ["error_type"]
        )
        self.memory_usage = Gauge("memory_usage_bytes", "Current memory usage in bytes")
        self.cpu_usage = Gauge("cpu_usage_percent", "Current CPU usage percentage")
        self.disk_usage = Gauge(
            "disk_usage_bytes", "Current disk usage in bytes", ["mount_point"]
        )
        self.network_io = Gauge(
            "network_io_bytes", "Network IO statistics", ["direction"]
        )
        self.request_queue_size = Gauge(
            "request_queue_size", "Current size of the request queue"
        )
        self.request_queue_latency = Histogram(
            "request_queue_latency_seconds",
            "Time requests spend in queue",
            buckets=(0.1, 0.5, 1.0, 2.5, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0),
        )
        self.weight_update_duration = Histogram(
            "weight_update_duration_seconds",
            "Time spent updating weights",
            buckets=(0.1, 0.5, 1.0, 2.5, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0),
        )
        self.weight_update_failures = Counter(
            "weight_update_failures_total",
            "Total number of weight update failures",
            ["reason"],
        )
        self.last_weight_update = Gauge(
            "last_weight_update_timestamp",
            "Timestamp of last successful weight update",
        )
        self.score_changes = Histogram(
            "score_changes",
            "Changes in miner scores",
            ["direction"],
            buckets=(-1.0, -0.5, -0.1, -0.01, 0.0, 0.01, 0.1, 0.5, 1.0),
        )
        self.score_distribution = Histogram(
            "score_distribution",
            "Distribution of miner scores",
            buckets=(0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0),
        )
        self.total_proofs_verified = Counter(
            "total_proofs_verified",
            "Total number of proofs successfully verified",
            ["model"],
        )
        self.total_requests_processed = Counter(
            "total_requests_processed",
            "Total number of requests processed",
            ["model", "status"],
        )
        self.avg_response_time = Gauge(
            "avg_response_time_seconds", "Moving average of response times", ["model"]
        )
        self.error_counter = Counter(
            "errors_total",
            "Total number of errors by type",
            ["error_type", "component"],
        )
        self.last_error_timestamp = Gauge(
            "last_error_timestamp",
            "Timestamp of last error by type",
            ["error_type", "component"],
        )

    def shutdown(self):
        self.server.shutdown()


_metrics: _PrometheusMetrics | None = None


def start_prometheus_logging(port: int) -> None:
    global _metrics
    _metrics = _PrometheusMetrics(port)


def stop_prometheus_logging() -> None:
    global _metrics
    if _metrics:
        _metrics.shutdown()
        _metrics = None


def log_validation_time(time: float) -> None:
    if _metrics:
        _metrics.validation_times.observe(time)


def log_response_times(response_times: list[float], model_name: str) -> None:
    if not _metrics or not response_times:
        return
    _metrics.response_times.labels("max", model_name).observe(max(response_times))
    _metrics.response_times.labels("min", model_name).observe(min(response_times))
    mean = statistics.mean(response_times)
    _metrics.response_times.labels("mean", model_name).observe(mean)
    _metrics.response_times.labels("median", model_name).observe(
        statistics.median(response_times)
    )
    _metrics.avg_response_time.labels(model_name).set(mean)
    _metrics.total_requests_processed.labels(model_name, "success").inc(
        len(response_times)
    )


def log_proof_sizes(proof_sizes: list[int], model_name: str) -> None:
    if not _metrics or not proof_sizes:
        return
    _metrics.proof_sizes.labels("max", model_name).observe(max(proof_sizes))
    _metrics.proof_sizes.labels("min", model_name).observe(min(proof_sizes))
    _metrics.proof_sizes.labels("mean", model_name).observe(
        statistics.mean(proof_sizes)
    )
    _metrics.proof_sizes.labels("median", model_name).observe(
        statistics.median(proof_sizes)
    )


def log_verification_ratio(value: float, model_name: str) -> None:
    if not _metrics:
        return
    _metrics.verification_ratio.labels(model_name).observe(value)
    if value > 0:
        _metrics.total_proofs_verified.labels(model_name).inc()


def log_verification_failure(model_name: str, failure_type: str) -> None:
    if not _metrics:
        return
    _metrics.verification_failures.labels(model_name, failure_type).inc()
    _metrics.total_requests_processed.labels(model_name, "failed").inc()


def log_timeout(model_name: str) -> None:
    if not _metrics:
        return
    _metrics.timeout_counter.labels(model_name).inc()
    _metrics.total_requests_processed.labels(model_name, "timeout").inc()


def log_network_error(error_type: str) -> None:
    if _metrics:
        _metrics.network_errors.labels(error_type).inc()


def log_system_metrics() -> None:
    if not _metrics:
        return
    _metrics.cpu_usage.set(psutil.cpu_percent())
    _metrics.memory_usage.set(psutil.Process().memory_info().rss)
    for partition in psutil.disk_partitions():
        usage = psutil.disk_usage(partition.mountpoint)
        _metrics.disk_usage.labels(partition.mountpoint).set(usage.used)
    net_io = psutil.net_io_counters()
    _metrics.network_io.labels("bytes_sent").set(net_io.bytes_sent)
    _metrics.network_io.labels("bytes_recv").set(net_io.bytes_recv)


def log_queue_metrics(queue_size: int, latency: float) -> None:
    if not _metrics:
        return
    _metrics.request_queue_size.set(queue_size)
    _metrics.request_queue_latency.observe(latency)


def log_weight_update(
    duration: float, success: bool = True, failure_reason: str = ""
) -> None:
    if not _metrics:
        return
    if success:
        _metrics.weight_update_duration.observe(duration)
        _metrics.last_weight_update.set_to_current_time()
    else:
        _metrics.weight_update_failures.labels(failure_reason).inc()


def log_score_change(old_score: float, new_score: float) -> None:
    if not _metrics:
        return
    change = new_score - old_score
    direction = "increase" if change > 0 else "decrease"
    _metrics.score_changes.labels(direction).observe(abs(change))
    _metrics.score_distribution.observe(new_score)


def log_error(error_type: str, component: str, error_msg: str) -> None:
    if not _metrics:
        return
    _metrics.error_counter.labels(error_type, component).inc()
    _metrics.last_error_timestamp.labels(error_type, component).set_to_current_time()
