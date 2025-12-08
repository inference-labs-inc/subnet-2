import sys
import bittensor as bt
from constants import DEFAULT_NETUID, COMPETITION_SYNC_INTERVAL

from utils import wandb_logger
from _validator.config.api import ApiConfig


class ValidatorConfig:
    """
    Configuration class for the Validator.

    This class initializes and manages the configuration settings for the validator.

    Attributes:
        config (bt.config): The Bittensor configuration object.
        subnet_uid (int): The unique identifier for the subnet.
        wallet (bt.wallet): The Bittensor wallet object.
        subtensor (bt.subtensor): The Bittensor subtensor object.
        dendrite (bt.dendrite): The Bittensor dendrite object.
        metagraph (bt.metagraph): The Bittensor metagraph object.
        user_uid (int): The unique identifier for the validator within the subnet's metagraph.
        api_enabled (bool): Whether the API is enabled.
    """

    def __init__(self, config: bt.config):
        """
        Initialize the ValidatorConfig object.

        Args:
            config (bt.config): The Bittensor configuration object.
        """
        for key, value in vars(config).items():
            setattr(self, key, value)

        self.bt_config: bt.Config = config
        self.subnet_uid = int(
            self.bt_config.netuid if self.bt_config.netuid else DEFAULT_NETUID
        )
        self.wallet = bt.wallet(config=self.bt_config)
        self.dendrite = bt.dendrite(wallet=self.wallet)
        self.subtensor = bt.subtensor(config=self.bt_config)
        try:
            self.metagraph = self.subtensor.metagraph(self.subnet_uid)
        except Exception as e:
            bt.logging.error(f"Error getting metagraph: {e}")
            self.metagraph = None
        self.user_uid = int(
            self.metagraph.hotkeys.index(self.wallet.hotkey.ss58_address)
        )
        self.localnet = self.bt_config.localnet
        self.api = ApiConfig(self.bt_config)
        self.competition_sync_interval = (
            COMPETITION_SYNC_INTERVAL
            if self.bt_config.competition_sync_interval is None
            else self.bt_config.competition_sync_interval
        )

        # Initialize wandb logger
        wandb_logger.safe_init(
            "Validator",
            self.wallet,
            self.metagraph,
            self.bt_config,
        )

    def check_register(self):
        """
        Check if the validator is registered on the subnet.

        This method verifies if the validator's hotkey is registered in the metagraph.
        If not registered, it logs an error and exits.
        If registered, it sets the user_uid and logs it.

        Raises:
            SystemExit: If the validator is not registered on the network.
        """
        if self.wallet.hotkey.ss58_address not in self.metagraph.hotkeys:
            bt.logging.error(
                f"\nYour validator: {self.wallet} is not registered to the chain: "
                f"{self.subtensor} \nRun btcli register and try again."
            )
            sys.exit(1)
        else:
            uid = self.metagraph.hotkeys.index(self.wallet.hotkey.ss58_address)
            bt.logging.info(f"Running validator on uid: {uid}")
            self.user_uid = uid
