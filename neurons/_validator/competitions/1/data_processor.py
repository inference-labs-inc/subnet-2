import torch
import torch.nn.functional as F

from _validator.competitions.services.data_source import (
    CompetitionDataProcessor,
)


class DatasetProcessor(CompetitionDataProcessor):
    def __init__(self, noise_scale: float = 0.01, jitter_scale: float = 0.1):
        self.noise_scale = noise_scale
        self.jitter_scale = jitter_scale

    def process(self, inputs: torch.Tensor) -> torch.Tensor:
        batch_size = inputs.shape[0]

        if torch.rand(1) > 0.5:
            inputs = torch.flip(inputs, dims=[3])

        jitter = (
            1.0
            + (torch.rand(batch_size, 1, 1, 1, device=inputs.device) * 2 - 1)
            * self.jitter_scale
        )
        inputs = inputs * jitter

        angle = (torch.rand(1) * 20 - 10) * (3.14159 / 180)
        cos_theta = torch.cos(angle)
        sin_theta = torch.sin(angle)
        rotation_matrix = torch.tensor(
            [[cos_theta, -sin_theta, 0], [sin_theta, cos_theta, 0]],
            device=inputs.device,
        )
        grid = F.affine_grid(
            rotation_matrix.unsqueeze(0).expand(batch_size, -1, -1),
            inputs.size(),
            align_corners=True,
        )
        inputs = F.grid_sample(inputs, grid, align_corners=True)

        noise = torch.randn_like(inputs) * self.noise_scale
        perturbed = inputs + noise

        return torch.clamp(perturbed, -1, 1)
