import argparse
import asyncio
import hashlib
import ssl
import websockets
import bittensor as bt
from OpenSSL import crypto

bt.logging.on()
bt.logging.set_console()


async def verify_validator_cert(
    netuid: int, validator_ss58: str, network: str = "test"
):

    subtensor = bt.subtensor(network=network)

    try:
        metagraph = subtensor.metagraph(netuid)
        bt.logging.debug(f"Loaded metagraph for netuid {netuid}")

        try:
            validator_uid = None
            for uid, hotkey in enumerate(metagraph.hotkeys):
                if hotkey == validator_ss58:
                    validator_uid = uid
                    break

            if validator_uid is None:
                bt.logging.error(
                    f"Validator {validator_ss58} not found in metagraph hotkeys"
                )
                return False

            bt.logging.info(f"Found validator UID: {validator_uid}")
            commitment = subtensor.get_commitment(netuid, validator_uid)
            if not commitment:
                bt.logging.error(
                    f"No certificate commitment found for validator {validator_ss58} (UID: {validator_uid})"
                )
                return False
            bt.logging.info(f"Found commitment: {commitment}")
        except Exception as e:
            bt.logging.error(f"Error processing validator: {str(e)}")
            return False

    except Exception as e:
        bt.logging.error(f"Failed to get validator commitment: {str(e)}")
        return False

    try:
        neuron = metagraph.neurons[validator_uid]
        if not neuron or not neuron.axon_info:
            bt.logging.error(
                f"No axon info found for validator {validator_ss58} (UID: {validator_uid})"
            )
            return False

        ip = neuron.axon_info.ip
        port = neuron.axon_info.port
        bt.logging.info(f"Found validator endpoint: {ip}:{port}")
    except Exception as e:
        bt.logging.error(f"Failed to get validator axon info: {str(e)}")
        return False

    ssl_context = ssl.create_default_context()
    ssl_context.check_hostname = False
    ssl_context.verify_mode = ssl.CERT_NONE

    uri = f"wss://{ip}:{port}/rpc"
    try:
        async with websockets.connect(uri, ssl=ssl_context) as websocket:
            ssl_socket = websocket.transport.get_extra_info("ssl_object")
            cert_bin = ssl_socket.getpeercert(binary_form=True)
            if not cert_bin:
                bt.logging.error("No certificate received from validator")
                return False

            cert = crypto.load_certificate(crypto.FILETYPE_ASN1, cert_bin)
            cert_der = crypto.dump_certificate(crypto.FILETYPE_ASN1, cert)
            cert_hash = hashlib.sha256(cert_der).hexdigest()
            bt.logging.info(f"Received certificate hash: {cert_hash}")

            if cert_hash != commitment:
                bt.logging.error("Certificate hash does not match commitment!")
                bt.logging.error(f"Expected: {commitment}")
                bt.logging.error(f"Got: {cert_hash}")
                return False

            bt.logging.success("Certificate verification successful!")
            return True

    except Exception as e:
        bt.logging.error(f"Failed to connect to validator: {str(e)}")
        return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--netuid", type=int, required=True, help="Subnet UID")
    parser.add_argument(
        "--validator", type=str, required=True, help="Validator SS58 address"
    )
    parser.add_argument(
        "--network",
        type=str,
        default="test",
        help="Subtensor network (test, finney, local...)",
    )
    args = parser.parse_args()

    result = asyncio.run(verify_validator_cert(args.netuid, args.validator))
    if not result:
        exit(1)


if __name__ == "__main__":
    main()
