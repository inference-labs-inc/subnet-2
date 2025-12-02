from _validator.models.base_rpc_request import QueuedRequestDataModel
from pydantic import Field
from deployment_layer.circuit_store import circuit_store
from execution_layer.circuit import CircuitType


class ProofOfComputationRPCRequest(QueuedRequestDataModel):
    """
    Request for the Proof of Computation RPC method.
    """

    circuit_id: str = Field(..., description="The ID of the circuit to use")

    class Config:
        arbitrary_types_allowed = True
        extra = "allow"

    def __init__(self, **data):
        circuit_id = data.get("circuit_id")
        if not circuit_id:
            raise ValueError("circuit_id is required")

        circuit = circuit_store.get_circuit(circuit_id)
        if circuit is None:
            raise ValueError(f"No circuit found for ID {circuit_id}")

        if circuit.metadata.type != CircuitType.PROOF_OF_COMPUTATION:
            raise ValueError(
                f"Circuit {circuit_id} is not a proof of computation circuit"
            )

        super().__init__(
            circuit=circuit, inputs=data.get("inputs"), circuit_id=circuit_id
        )
