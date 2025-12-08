import bittensor as bt
import base64
import time
import argparse

bt.logging.on()
bt.logging.set_console()


def sign_timestamp(wallet_name: str, hotkey_name: str) -> tuple[str, str, str]:
    """
    Signs the current timestamp using a bittensor wallet's hotkey.

    Args:
        wallet_name: Name of the wallet to use
        hotkey_name: Name of the hotkey to use

    Returns:
        tuple containing (timestamp, ss58_address, base64_signature)
    """
    wallet = bt.wallet(name=wallet_name, hotkey=hotkey_name)
    timestamp = str(int(time.time()))
    signature = wallet.hotkey.sign(timestamp.encode())
    return timestamp, wallet.hotkey.ss58_address, base64.b64encode(signature).decode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--wallet", type=str, required=True, help="Name of the wallet")
    parser.add_argument("--hotkey", type=str, required=True, help="Name of the hotkey")
    args = parser.parse_args()

    try:
        timestamp, ss58_address, signature = sign_timestamp(args.wallet, args.hotkey)
        print("\nAPI Request Headers:")
        print(f"x-timestamp: {timestamp}")
        print(f"x-origin-ss58: {ss58_address}")
        print(f"x-signature: {signature}")
        print("\nThese headers are valid for API requests to the validator.")
    except Exception as e:
        print(f"Error: {str(e)}")


if __name__ == "__main__":
    main()
