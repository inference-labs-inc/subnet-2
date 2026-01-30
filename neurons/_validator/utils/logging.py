from __future__ import annotations

import torch
from rich.console import Console, JustifyMethod
from rich.table import Table

from utils import wandb_logger
from _validator.models.miner_response import MinerResponse


def create_and_print_table(
    title: str, columns: list[tuple[str, JustifyMethod, str]], rows: list[list[str]]
):
    """
    Create and print a table.

    Args:
        title (str): The title of the table.
        columns (list[tuple[str, JustifyMethod, str]]): A list of tuples containing column information.
            Each tuple should contain (column_name, justification, style).
        rows (list[list[str]]): A list of rows, where each row is a list of string values.

    """
    table = Table(title=title)
    for col_name, justify, style in columns:
        table.add_column(col_name, justify=justify, style=style, no_wrap=True)
    for row in rows:
        table.add_row(*row)
    console = Console(color_system="truecolor")
    console.width = 120
    console.print(table)


def log_tensor_data(title: str, data: torch.Tensor, log_key: str):
    """
    Log tensor data to a table and Weights & Biases.

    Args:
        title (str): The title of the table.
        data (torch.Tensor): The tensor data to be logged.
        log_key (str): The key used for logging in Weights & Biases.
    """
    rows = [[str(uid), f"{value.item():.6f}"] for uid, value in enumerate(data)]
    create_and_print_table(
        title, [("uid", "right", "cyan"), (log_key, "right", "yellow")], rows
    )
    wandb_logger.safe_log(
        {log_key: {uid: value.item() for uid, value in enumerate(data)}}
    )


def log_scores(scores: torch.Tensor):
    """
    Log scores to a table and Weights & Biases.

    Args:
        scores (torch.Tensor): The scores tensor to be logged.

    """
    log_tensor_data("scores", scores, "scores")


def log_weights(weights: torch.Tensor):
    """
    Log weights to a table and Weights & Biases.

    Args:
        weights (torch.Tensor): The weights tensor to be logged.
    """
    log_tensor_data("weights", weights, "weights")


def log_verify_result(results: list[tuple[int, bool]]):
    """
    Log verification results to a table and Weights & Biases.

    Args:
        results (list[tuple[int, bool]]): A list of tuples containing (uid, verification_result).

    """
    rows = [[str(uid), str(result)] for uid, result in results]
    create_and_print_table(
        "proof verification result",
        [("uid", "right", "cyan"), ("Verified?", "right", "green")],
        rows,
    )
    wandb_logger.safe_log(
        {"verification_results": {uid: int(result) for uid, result in results}}
    )


def log_responses(responses: list[MinerResponse]):
    """
    Log miner responses to a table and Weights & Biases.

    Args:
        responses (list[MinerResponse]): A list of MinerResponse objects to be logged.
    """
    columns = [
        ("UID", "right", "cyan"),
        ("Verification Result", "right", "green"),
        ("Response Time", "right", "yellow"),
        ("Proof Size", "right", "blue"),
        ("Circuit Name", "left", "magenta"),
        ("Proof System", "left", "red"),
        ("Request Type", "left", "white"),
    ]

    sorted_responses = sorted(responses, key=lambda x: x.uid)
    rows = [
        [
            str(response.uid),
            str(response.verification_result),
            str(response.response_time),
            str(response.proof_size),
            (response.circuit.metadata.name if response.circuit else "Unknown"),
            (response.proof_system or "Unknown"),
            (response.request_type.name if response.request_type else "Unknown"),
        ]
        for response in sorted_responses
    ]
    create_and_print_table("Responses", columns, rows)

    wandb_log = {
        "responses": {
            response.uid: {
                str(response.circuit): {
                    "verification_result": int(response.verification_result),
                    "response_time": response.response_time,
                    "proof_size": response.proof_size,
                }
            }
            for response in sorted_responses
            if response.verification_result
        }
    }
    wandb_logger.safe_log(wandb_log)
