from deployment_layer.model_50818a54b31b3e0fe3306a7fb7826156fc2c42c9d64c6ba106ba135fbe7b7b19.input import (
    CircuitInput,
)
from _validator.models.request_type import RequestType


def main():
    input_instance = CircuitInput(RequestType.BENCHMARK)
    generated_data = input_instance.generate()
    print(f"Generated list items: {generated_data['list_items']}")


if __name__ == "__main__":
    main()
