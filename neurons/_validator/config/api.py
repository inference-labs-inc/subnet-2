import bittensor as bt


class ApiConfig:
    """
    Configuration class for the API.

    Attributes:
        enabled (bool): Whether the API is enabled.
        host (str): The host for the API.
        port (int): The port for the API.
        workers (int): The number of workers for the API.
        verify_external_signatures (bool): Whether to verify external signatures.
        certificate_path (str): The path to the certificate directory.
        serve_axon (bool): Whether to serve the axon displaying your API information.
    """

    def __init__(self, config: bt.config):
        self.enabled = not config.ignore_external_requests
        self.host = config.external_api_host
        self.port = config.external_api_port
        self.workers = config.external_api_workers
        self.verify_external_signatures = not config.do_not_verify_external_signatures
        self.certificate_path = config.certificate_path
        self.whitelisted_public_keys = config.whitelisted_public_keys
        self.serve_axon = config.serve_axon
