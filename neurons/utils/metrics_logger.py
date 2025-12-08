from utils import wandb_logger


def log_circuit_metrics(
    response_times: list[float], verified_count: int, circuit_name: str
) -> None:
    """
    Log circuit-specific metrics to wandb.

    Args:
        response_times (list[float]): List of response times for successful verifications
        verified_count (int): Number of verified responses
        circuit_name (str): Name of the circuit
    """
    if response_times:
        max_response_time = max(response_times)
        min_response_time = min(response_times)
        mean_response_time = sum(response_times) / len(response_times)
        median_response_time = sorted(response_times)[len(response_times) // 2]
        wandb_logger.safe_log(
            {
                f"{circuit_name}": {
                    "max_response_time": max_response_time,
                    "min_response_time": min_response_time,
                    "mean_response_time": mean_response_time,
                    "median_response_time": median_response_time,
                    "total_responses": len(response_times),
                    "verified_responses": verified_count,
                }
            }
        )
