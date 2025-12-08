from execution_layer.proof_handlers.circom_handler import CircomHandler
from execution_layer.circuit import ProofSystem
from execution_layer.proof_handlers.jolt_handler import JoltHandler
from execution_layer.proof_handlers.ezkl_handler import EZKLHandler


class ProofSystemFactory:
    _handlers = {
        ProofSystem.CIRCOM: CircomHandler,
        ProofSystem.JOLT: JoltHandler,
        ProofSystem.EZKL: EZKLHandler,
    }

    @classmethod
    def get_handler(cls, proof_system):
        if isinstance(proof_system, str):
            try:
                proof_system = ProofSystem[proof_system.upper()]
            except KeyError as e:
                raise ValueError(f"Invalid proof system string: {proof_system}") from e

        handler_class = cls._handlers.get(proof_system)
        if handler_class is None:
            raise ValueError(f"Unsupported proof system: {proof_system}")
        return handler_class()
