import os
import sys
import json
import torch
import numpy as np
import matplotlib.pyplot as plt
from matplotlib.colors import LinearSegmentedColormap
from pathlib import Path
from collections import defaultdict
from typing import Dict, List, Optional, Union
from rich.console import Console
from rich.table import Table
import argparse

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def load_circuit_names(deployment_layer_path: str) -> Dict[str, str]:
    circuits: Dict[str, str] = {}
    for folder_name in os.listdir(deployment_layer_path):
        folder_path = os.path.join(deployment_layer_path, folder_name)
        if os.path.isdir(folder_path) and folder_name.startswith("model_"):
            circuit_id = folder_name.split("_")[1]
        try:
            with open(os.path.join(folder_path, "metadata.json"), "r") as f:
                circuit_data = json.load(f)
                circuit_name = circuit_data["name"]
                circuits[circuit_id] = circuit_name
        except Exception:
            continue

    return circuits


def load_scores(scores_path: Path) -> Union[List, Dict]:
    if not scores_path.exists():
        return {}
    try:
        scores = torch.load(scores_path)
        if isinstance(scores, torch.Tensor):
            return scores.tolist()
        return scores
    except Exception as e:
        print(f"Error loading scores from {scores_path}: {str(e)}")
        return {}


def parse_args():
    parser = argparse.ArgumentParser(description="Plot model statistics")
    parser.add_argument(
        "--path",
        type=str,
        default=os.path.expanduser("~/.bittensor/subnet-2"),
        help="Path to bittensor/subnet-2 directory",
    )
    return parser.parse_args()


def process_evaluation_data(
    eval_file: Path,
    model_id: str,
    scores: Union[List, Dict],
    circuit_name: Optional[str] = None,
) -> Optional[Dict]:
    if not eval_file.exists():
        return None

    try:
        with open(eval_file, "r") as f:
            data = json.load(f)
            if not data:
                return None

            uid_latest = {}
            seen_uids = set()

            for item in data:
                if item["verification_result"]:
                    uid = item["uid"]
                    if uid in seen_uids:
                        print(
                            f"\nWarning: Found duplicate UID {uid} in model {model_id}, keeping most recent entry"
                        )
                    seen_uids.add(uid)

                    score = item.get("score", 0.0)
                    uid_latest[uid] = (uid, item["response_time"], score)

            if not uid_latest:
                return None

            response_times = list(uid_latest.values())
            uids, times, scores = zip(*response_times)
            return {
                "uids": uids,
                "times": times,
                "scores": scores,
                "min_time": min(times),
                "max_time": max(times),
                "circuit": circuit_name,
            }
    except (json.JSONDecodeError, KeyError) as e:
        print(f"Error processing evaluation data for model {model_id}: {str(e)}")
        return None


def load_evaluation_data(models_path: Path, scores_path: Path) -> Dict:

    scores = load_scores(scores_path)

    model_stats = {}

    for model_dir in models_path.glob("model_*"):
        model_id = model_dir.name.replace("model_", "")
        eval_file = model_dir / "evaluation_data.json"
        circuit_names = load_circuit_names(os.path.join("neurons", "deployment_layer"))
        circuit_name = circuit_names.get(model_id)
        if not circuit_name:
            print(f"Warning: No circuit found for model {model_id}")
            continue

        stats = process_evaluation_data(eval_file, model_id, scores, circuit_name)
        if stats:
            model_stats[model_id] = stats

    return dict(
        sorted(model_stats.items(), key=lambda x: (x[1]["min_time"], x[1]["max_time"]))
    )


def create_scatter_plot(
    ax: plt.Axes,
    data: Dict,
    title: str = None,
    score_plot: bool = False,
    score_range: tuple = None,
) -> None:
    if title == "Average Response Time Across All Models":
        ax.set_title(title, fontsize=14, pad=15, weight="bold")
    elif title == "Average Scores Across All Models":
        ax.set_title(title, fontsize=14, pad=15, weight="bold")
    else:
        circuit_name = data.get("circuit", "Unknown Circuit")
        ax.set_title(f"{circuit_name}", fontsize=14, pad=15, weight="bold")

    times = np.array(data["times"])
    scores = np.array(data["scores"])

    if score_plot:
        y_values = scores
        sort_idx = np.argsort(scores)[::-1]
        color_values = times
        ylabel = "Score"
        cbar_label = "Response Time (s)"
    else:
        y_values = times
        sort_idx = np.argsort(times)[::-1]
        color_values = scores
        ylabel = "Response Time (s)"
        cbar_label = "Score"

    sorted_y = y_values[sort_idx]
    sorted_colors = color_values[sort_idx]

    colors = [(0.8, 0.1, 0.1), (0.95, 0.9, 0.25), (0.1, 0.8, 0.1)]
    n_bins = 100
    cmap = LinearSegmentedColormap.from_list("custom", colors, N=n_bins)

    if len(sorted_colors) > 0:
        if score_plot:

            color_range = sorted_colors.max() - sorted_colors.min()
            if color_range > 0:
                normalized_colors = (sorted_colors - sorted_colors.min()) / color_range
            else:
                normalized_colors = np.zeros_like(sorted_colors)
        else:

            if score_range:
                min_score, max_score = score_range
                score_range_diff = max_score - min_score
                if score_range_diff > 0:
                    normalized_colors = (sorted_colors - min_score) / score_range_diff
                else:
                    normalized_colors = np.zeros_like(sorted_colors)
            else:

                color_range = sorted_colors.max() - sorted_colors.min()
                if color_range > 0:
                    normalized_colors = (
                        sorted_colors - sorted_colors.min()
                    ) / color_range
                else:
                    normalized_colors = np.zeros_like(sorted_colors)
    else:
        normalized_colors = np.array([])

    x_range = np.arange(len(sorted_y))
    scatter = ax.scatter(
        x_range,
        sorted_y,
        c=normalized_colors,
        cmap=cmap,
        alpha=0.9,
        s=100,
        edgecolor="white",
        linewidth=0.5,
    )
    cbar = plt.colorbar(scatter, label=cbar_label)
    cbar.ax.tick_params(labelsize=8)
    cbar.ax.set_facecolor("#f8f9fa")

    if score_plot and score_range:
        min_score, max_score = score_range
        ax.set_ylim(min_score * 0.95, max_score * 1.05)

    ax.set_ylabel(ylabel, fontsize=12, weight="bold")
    ax.grid(True, alpha=0.2, linestyle="--", color="#cccccc")
    ax.tick_params(axis="both", labelsize=10)
    ax.set_facecolor("#f8f9fa")

    xlabel = (
        "UIDs (sorted by score)" if score_plot else "UIDs (sorted by response time)"
    )
    ax.set_xlabel(xlabel, fontsize=12, weight="bold")

    for spine in ax.spines.values():
        spine.set_edgecolor("#dddddd")
        spine.set_linewidth(1.5)


def calculate_average_times(model_stats: Dict, scores: List) -> Dict:
    uid_times = defaultdict(list)
    uid_scores = {}

    for stats in model_stats.values():
        for uid, time in zip(stats["uids"], stats["times"]):
            uid_times[uid].append(time)
            if uid < len(scores):
                uid_scores[uid] = scores[uid]
            else:
                uid_scores[uid] = 0.0

    sorted_uids = sorted(uid_times.keys(), key=lambda x: np.mean(uid_times[x]))
    avg_times = [np.mean(uid_times[uid]) for uid in sorted_uids]
    final_scores = [uid_scores[uid] for uid in sorted_uids]

    console = Console()
    table = Table(title="Top and Bottom Response Times")
    table.add_column("Position", justify="center", style="cyan")
    table.add_column("UID", justify="center", style="green")
    table.add_column("Avg Response Time (s)", justify="center", style="yellow")
    table.add_column("Score", justify="center", style="magenta")

    num_to_show = min(5, len(sorted_uids))
    for i in range(num_to_show):
        table.add_row(
            f"Top {i + 1}",
            str(sorted_uids[i]),
            f"{avg_times[i]:.4f}",
            f"{final_scores[i]:.4f}",
        )

    table.add_section()

    for i in range(num_to_show):
        idx = -(i + 1)
        table.add_row(
            f"Bottom {i + 1}",
            str(sorted_uids[idx]),
            f"{avg_times[idx]:.4f}",
            f"{final_scores[idx]:.4f}",
        )

    console.print(table)

    return {
        "uids": sorted_uids,
        "times": avg_times,
        "scores": final_scores,
    }


def plot_stats(models_path: Path, scores_path: Path) -> None:
    print(f"\nLoading model stats from {models_path}")
    scores = load_scores(scores_path)
    model_stats = load_evaluation_data(models_path, scores_path)

    if not model_stats:
        print("No evaluation data found")
        return

    if scores:
        score_range = (min(scores), max(scores))
    else:
        score_range = (0, 1)

    print(f"\nProcessing {len(model_stats)} models...")
    n_models = len(model_stats)

    plt.rcParams["figure.facecolor"] = "white"
    plt.rcParams["axes.facecolor"] = "#f8f9fa"
    plt.rcParams["axes.grid"] = True
    plt.rcParams["grid.alpha"] = 0.2
    plt.rcParams["grid.color"] = "#cccccc"

    fig = plt.figure(figsize=(20, 5 * (n_models + 2)))
    fig.patch.set_facecolor("#ffffff")

    for idx, (model_id, stats) in enumerate(model_stats.items(), 1):
        circuit_name = stats.get("circuit", f"Model {model_id}")
        print(f"Plotting {circuit_name} ({idx}/{n_models})")
        ax = plt.subplot(n_models + 2, 1, idx)
        create_scatter_plot(ax, stats)
        if idx != n_models:
            ax.set_xticklabels([])

    print("Calculating average times...")
    avg_stats = calculate_average_times(model_stats, scores)

    ax = fig.add_subplot(n_models + 2, 1, n_models + 1)
    create_scatter_plot(ax, avg_stats, "Average Response Time Across All Models")

    ax = fig.add_subplot(n_models + 2, 1, n_models + 2)
    create_scatter_plot(
        ax,
        avg_stats,
        "Average Scores Across All Models",
        score_plot=True,
        score_range=score_range,
    )

    print("Saving plot...")
    plt.tight_layout(pad=3.0)
    plt.savefig("model_stats.png", dpi=300, bbox_inches="tight", facecolor="white")
    plt.close()
    print("Done! Plot saved as model_stats.png")


if __name__ == "__main__":
    args = parse_args()
    base_path = Path(args.path)
    models_path = base_path / "models"
    scores_path = base_path / "scores/scores.pt"

    if not models_path.exists():
        print(f"Models directory not found: {models_path}")
        sys.exit(1)

    if not scores_path.exists():
        print(f"Scores file not found: {scores_path}")
        print("Will proceed with empty scores...")

    plot_stats(models_path, scores_path)
