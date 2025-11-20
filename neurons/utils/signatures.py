from dataclasses import dataclass

from substrateinterface import Keypair


@dataclass(frozen=True)
class Headers:
    EXCHANGE_SYMMETRIC_KEY_ENDPOINT: str = "exchange-symmetric-key"
    PUBLIC_ENCRYPTION_KEY_ENDPOINT: str = "public-encryption-key"
    SYMMETRIC_KEY_UUID: str = "symmetric-key-uuid"
    HEADER_HASH: str = "header-hash"
    HOTKEY: str = "hotkey"
    MINER_HOTKEY: str = "miner-hotkey"
    VALIDATOR_HOTKEY: str = "validator-hotkey"
    NEURON_INFO_LITE: str = "NeuronInfoLite"
    NONCE: str = "nonce"
    SIGNATURE: str = "signature"


def sign_message(private_key: bytes | str, message: str | None) -> str | None:
    keypair = Keypair(private_key=private_key)
    if message is None:
        return None
    return f"0x{keypair.sign(message).hex()}"


def verify_signature(
    message: str | None, signature: str, signer_ss58_address: str
) -> bool:
    if message is None:
        return False
    try:
        keypair = Keypair(ss58_address=signer_ss58_address)
        return keypair.verify(data=message, signature=signature)
    except ValueError:
        return False
