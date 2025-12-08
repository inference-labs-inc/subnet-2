import torch
import torch.nn as nn
import torch.onnx
import json
import subprocess
import logging


class DummyModel(nn.Module):
    def __init__(self):
        super(DummyModel, self).__init__()
        self.conv = nn.Conv2d(3, 3, 1)

    def forward(self, x):
        x = self.conv(x)
        # Stage 1 outputs
        prob_stage_1 = torch.ones(x.shape[0], 3)
        prob_stage_2 = torch.ones(x.shape[0], 3)
        prob_stage_3 = torch.ones(x.shape[0], 3)
        stage1_delta_k = torch.ones(x.shape[0], 1)
        stage2_delta_k = torch.ones(x.shape[0], 1)
        stage3_delta_k = torch.ones(x.shape[0], 1)
        index_offset_stage1 = torch.ones(x.shape[0], 3)
        index_offset_stage2 = torch.ones(x.shape[0], 3)
        index_offset_stage3 = torch.ones(x.shape[0], 3)
        return (
            prob_stage_1,
            prob_stage_2,
            prob_stage_3,
            stage1_delta_k,
            stage2_delta_k,
            stage3_delta_k,
            index_offset_stage1,
            index_offset_stage2,
            index_offset_stage3,
        )


model = DummyModel()
dummy_input = torch.randn(1, 3, 64, 64)

# Export ONNX model
torch.onnx.export(
    model,
    dummy_input,
    "network.onnx",
    input_names=["input"],
    output_names=[
        "prob_stage_1",
        "prob_stage_2",
        "prob_stage_3",
        "stage1_delta_k",
        "stage2_delta_k",
        "stage3_delta_k",
        "index_offset_stage1",
        "index_offset_stage2",
        "index_offset_stage3",
    ],
    dynamic_axes={
        "input": {0: "batch_size"},
        "prob_stage_1": {0: "batch_size"},
        "prob_stage_2": {0: "batch_size"},
        "prob_stage_3": {0: "batch_size"},
        "stage1_delta_k": {0: "batch_size"},
        "stage2_delta_k": {0: "batch_size"},
        "stage3_delta_k": {0: "batch_size"},
        "index_offset_stage1": {0: "batch_size"},
        "index_offset_stage2": {0: "batch_size"},
        "index_offset_stage3": {0: "batch_size"},
    },
)
input_data = {
    "input_data": [dummy_input.numpy().flatten().tolist()],
}

with open("input.json", "w") as f:
    json.dump(input_data, f)

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

commands = [
    "ezkl gen-settings",
    "ezkl calibrate-settings --data input.json",
    "ezkl compile-circuit",
    "ezkl setup",
    "ezkl gen-witness --data input.json",
    "ezkl prove",
    "ezkl verify",
]

for cmd in commands:
    logger.info(f"Running command: {cmd}")
    try:
        result = subprocess.run(cmd.split(), capture_output=True, text=True)
        logger.info(f"Output:\n{result.stdout}")
        if result.stderr:
            logger.error(f"Errors:\n{result.stderr}")
        if result.returncode != 0:
            logger.error(f"Command failed with return code {result.returncode}")
            break
    except Exception as e:
        logger.error(f"Failed to run command: {e}")
        break
