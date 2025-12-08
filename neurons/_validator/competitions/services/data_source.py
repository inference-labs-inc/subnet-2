from abc import ABC, abstractmethod
from typing import Optional
import torch
import os
import json
import numpy as np
import bittensor as bt
from urllib.parse import urlparse
import requests
import random
import zipfile
import cv2
from tqdm import tqdm


class ImageProcessor:
    @staticmethod
    def normalize(
        img: torch.Tensor, mean=[0.485, 0.456, 0.406], std=[0.229, 0.224, 0.225]
    ) -> torch.Tensor:
        mean = torch.tensor(mean).view(-1, 1, 1)
        std = torch.tensor(std).view(-1, 1, 1)
        return (img - mean) / std

    @staticmethod
    def to_tensor(img: np.ndarray) -> torch.Tensor:
        img = img.transpose((2, 0, 1))  # HWC to CHW
        img = torch.from_numpy(img).float()
        return img / 255.0


class CompetitionDataProcessor(ABC):
    @abstractmethod
    def process(self, data: torch.Tensor) -> torch.Tensor:
        pass


class DefaultDataProcessor(CompetitionDataProcessor):
    def process(self, data: torch.Tensor) -> torch.Tensor:
        return data


class CompetitionDataSource(ABC):
    def __init__(
        self,
        competition_directory: str,
        processor: Optional[CompetitionDataProcessor] = None,
    ):
        self.competition_directory = competition_directory
        self.processor = processor or DefaultDataProcessor()
        self._load_config()

    def _load_config(self):
        config_path = os.path.join(
            self.competition_directory, "competition_config.json"
        )
        try:
            with open(config_path) as f:
                self.config = json.load(f)
        except Exception as e:
            bt.logging.error(f"Error loading competition config: {e}")
            self.config = {}

    @abstractmethod
    def get_benchmark_data(self) -> Optional[torch.Tensor]:
        pass

    @abstractmethod
    def sync_data(self) -> bool:
        pass


class RandomDataSource(CompetitionDataSource):
    def get_benchmark_data(self) -> Optional[torch.Tensor]:
        try:
            input_shape = self.config["circuit_settings"]["input_shape"]
            data = torch.randn(*input_shape)
            return self.processor.process(data)
        except Exception as e:
            bt.logging.error(f"Error generating random data: {e}")
            return None

    def sync_data(self) -> bool:
        return True


class RemoteDataSource(CompetitionDataSource):
    def __init__(
        self, competition_directory: str, processor: CompetitionDataProcessor = None
    ):
        super().__init__(competition_directory, processor)
        self.processed_path = os.path.join(self.competition_directory, "processed_64")
        self.data_config = self.config.get("data_source", {})

    def sync_data(self) -> bool:
        try:
            if os.path.exists(self.processed_path) and os.listdir(self.processed_path):
                return True

            os.makedirs(self.processed_path, exist_ok=True)
            extracted_path = os.path.join(self.competition_directory, "extracted")
            os.makedirs(extracted_path, exist_ok=True)
            zip_path = os.path.join(self.competition_directory, "age.zip")

            url = self.data_config.get(
                "url", "https://sn2-storage.inferencelabs.com/age.zip"
            )
            if not self._validate_url(url):
                bt.logging.error(f"Invalid URL: {url}")
                return False

            bt.logging.info("Downloading dataset...")
            response = requests.get(url, stream=True, timeout=900)
            total_size = int(response.headers.get("content-length", 0))

            with (
                open(zip_path, "wb") as f,
                tqdm(
                    desc="Downloading", total=total_size, unit="iB", unit_scale=True
                ) as pbar,
            ):
                for data in response.iter_content(chunk_size=1024):
                    size = f.write(data)
                    pbar.update(size)

            bt.logging.info("Extracting zip...")
            with zipfile.ZipFile(zip_path, "r") as zip_ref:
                zip_ref.extractall(extracted_path)

            bt.logging.info("Processing images to 64x64...")
            for root, _, files in tqdm(os.walk(extracted_path)):
                for img_name in files:
                    if img_name.lower().endswith((".png", ".jpg", ".jpeg")):
                        img_path = os.path.join(root, img_name)
                        try:
                            img = cv2.imread(img_path)
                            if img is not None:
                                img = cv2.resize(img, (64, 64))
                                cv2.imwrite(
                                    os.path.join(self.processed_path, img_name), img
                                )
                        except Exception as e:
                            bt.logging.warning(f"Failed to process {img_name}: {e}")

            os.remove(zip_path)
            import shutil

            shutil.rmtree(extracted_path)

            bt.logging.info(f"Dataset processed and saved to {self.processed_path}")
            return True

        except Exception as e:
            bt.logging.error(f"Failed to download/process dataset: {e}")
            return False

    def _validate_url(self, url: str) -> bool:
        try:
            parsed = urlparse(url)
            return any(
                domain in parsed.netloc
                for domain in [
                    "r2.cloudflarestorage.com",
                    "s3.amazonaws.com",
                    ".r2.dev",
                    "sn2-storage.inferencelabs.com",
                ]
            )
        except Exception:
            return False

    def get_benchmark_data(self) -> Optional[torch.Tensor]:
        try:
            if not os.path.exists(self.processed_path):
                if not self.sync_data():
                    return None

            image_files = [
                f
                for f in os.listdir(self.processed_path)
                if f.lower().endswith((".png", ".jpg", ".jpeg"))
            ]

            if not image_files:
                bt.logging.error("No processed images found")
                return None

            # Randomly select an image
            img_path = os.path.join(self.processed_path, random.choice(image_files))
            img = cv2.imread(img_path)
            if img is None:
                bt.logging.error(f"Failed to load image: {img_path}")
                return None

            # Convert BGR to RGB
            img = cv2.cvtColor(img, cv2.COLOR_BGR2RGB)

            # Convert to tensor and normalize
            tensor = ImageProcessor.to_tensor(img)
            tensor = ImageProcessor.normalize(tensor)

            # Add batch dimension
            tensor = tensor.unsqueeze(0)

            return self.processor.process(tensor)

        except Exception as e:
            bt.logging.error(f"Error getting benchmark data: {e}")
            return None
