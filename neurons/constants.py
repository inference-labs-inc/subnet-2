import os
from dataclasses import dataclass


@dataclass
class Roles:
    VALIDATOR = "validator"
    MINER = "miner"


# The model ID for a batched proof of weights model
BATCHED_PROOF_OF_WEIGHTS_MODEL_ID = (
    "1550853037e01d93c0831e2a4f80de7811b1c6780fb36b3cee89f4ba524df1be"
)
# The model ID for a single proof of weights model
SINGLE_PROOF_OF_WEIGHTS_MODEL_ID = (
    "4a87c995300f4e9ad9add9d5b800eb93bb3ecd3f9459b617f9924a211407a88c"
)

IGNORED_MODEL_HASHES = [
    "0",
    "0a92bc32ea02abe54159da70aeb541d52c3cba27c8708669eda634e096a86f8b",
    "b7d33e7c19360c042d94c5a7360d7dc68c36dd56c449f7c49164a0098769c01f",
    "55de10a6bcf638af4bc79901d63204a9e5b1c6534670aa03010bae6045e3d0e8",
    "9998a12b8194d3e57d332b484ede57c3d871d42a176456c4e10da2995791d181",
    "ed8ba401d709ee31f6b9272163c71451da171c7d71800313fe5db58d0f6c483a",
    "37320fc74fec80805eedc8e92baf3c58842a2cb2a4ae127ad6e930f0c8441c7a",
    "1d60d545b7c5123fd60524dcbaf57081ca7dc4a9ec36c892927a3153328d17c0",
    "33b92394b18412622adad75733a6fc659b4e202b01ee8a5465958a6bad8ded62",
    "8dcff627a782525ea86196941a694ffbead179905f0cd4550ddc3df9e2b90924",
    "a4bcecaf699fd9212600a1f2fcaa40c444e1aeaab409ea240a38c33ed356f4e2",
    "e84b2e5f223621fa20078eb9f920d8d4d3a4ff95fa6e2357646fdbb43a2557c9",
    "a849500803abdbb86a9460e18684a6411dc7ae0b75f1f6330e3028081a497dea",
    "f5b6043594f46ae6bd176ce60c7a099291cc6a3f6436fecd46142b1b1ecca5fb",
    "1e6fcdaea58741e7248b631718dda90398a17b294480beb12ce8232e27ca3bff",
    "fa0d509d52abe2d1e809124f8aba46258a02f7253582f7b7f5a22e1e0bca0dfb",
]

# The maximum timespan allowed for miners to respond to a query
VALIDATOR_REQUEST_TIMEOUT_SECONDS = 120
# The maximum timespan allowed for miners to process through a circuit
CIRCUIT_TIMEOUT_SECONDS = 60
# Whether to penalize miners for missing resets
RESET_PENALTY_ENABLED = False
# An additional queueing time for external requests
EXTERNAL_REQUEST_QUEUE_TIME_SECONDS = 10
# Maximum number of concurrent requests that the validator will handle
MAX_CONCURRENT_REQUESTS = 16
# Default proof size when we're unable to determine the actual size
DEFAULT_PROOF_SIZE = 5000
# Size in percent of the sample to be used for the maximum score median
MAXIMUM_SCORE_MEDIAN_SAMPLE = 0.05
# Shift in seconds to apply to the minimum response time for vertical asymptote adjustment
MINIMUM_SCORE_SHIFT = 0.0
# Weights version hyperparameter
WEIGHTS_VERSION = 11002
# Rate limit for weight updates
WEIGHT_RATE_LIMIT: int = 100
# Delay between loop iterations
LOOP_DELAY_SECONDS = 0.1
# Exception delay for loop
EXCEPTION_DELAY_SECONDS = 10
# Default maximum score
DEFAULT_MAX_SCORE = 1 / 235
# Default subnet UID
DEFAULT_NETUID = 2
# Validator stake threshold
VALIDATOR_STAKE_THRESHOLD = 1024
# 🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩🥩
STEAK = "🥩"
# Field modulus
FIELD_MODULUS = (
    21888242871839275222246405745257275088548364400416034343698204186575808495617
)
# Number of miner groups for reset events
NUM_MINER_GROUPS = 8
# Subnet tempo for epochs
EPOCH_TEMPO = 360
# Weight update buffer
WEIGHT_UPDATE_BUFFER = 15
# How close to the end of the epoch the boost is applied
BOOST_BUFFER = 50
# The window in blocks before an epoch boundary where a miner can reset.
MINER_RESET_WINDOW_BLOCKS = 10
# Whether on-chain proof of weights is enabled by default
ONCHAIN_PROOF_OF_WEIGHTS_ENABLED = False
# Frequency in terms of blocks at which proof of weights are posted
PROOF_OF_WEIGHTS_INTERVAL = 1000
# Maximum number of proofs to log at once
MAX_PROOFS_TO_LOG = 0
# Era period for proof of weights (mortality of the pow log)
PROOF_OF_WEIGHTS_LIFESPAN = 2
# Active competition
ACTIVE_COMPETITION = 0
# Frequency in terms of seconds at which the competition is synced and evaluated
COMPETITION_SYNC_INTERVAL = 60 * 60 * 24
# Maximum signature lifespan for WebSocket requests
MAX_SIGNATURE_LIFESPAN = 300
# Whitelisted public keys (ss58 addresses) we accept external requests from by default
# (even if an address is not in the metagraph)
WHITELISTED_PUBLIC_KEYS = []
# Mainnet <> Testnet UID mapping
MAINNET_TESTNET_UIDS = [
    (1, 61),  # apex
    (2, 118),  # subnet 2
    (3, 223),  # templar
    (4, 40),  # targon
    (5, 88),  # kaito
    (6, 155),  # infinite
    (7, 92),  # subvortex
    (8, 3),  # ptn
    (8, 116),  # ptn (PTN)
    (10, 104),  # sturdy
    (11, 135),  # dippy
    (12, 174),  # horde
    (13, 254),  # dataverse
    (14, 203),  # palaidn
    (15, 202),  # deval
    (16, 120),  # bitads
    (17, 89),  # 3gen
    (18, 24),  # cortex
    (19, 176),  # inference
    (20, 76),  # bitagent
    (21, 157),  # any-any
    (23, 119),  # social
    (24, 96),  # omega
    (25, 141),  # protein
    (26, 25),  # alchemy
    (27, 15),  # compute
    (28, 93),  # oracle
    (31, 123),  # naschain
    (32, 87),  # itsai
    (33, 138),  # ready
    (34, 168),  # mind
    (35, 78),  # logic
    (39, 159),  # edge
    (40, 166),  # chunk
    (41, 172),  # sportstensor
    (42, 165),  # masa
    (43, 65),  # graphite
    (44, 180),  # score
    (45, 171),  # gen42
    (46, 182),  # neural
    (48, 208),  # nextplace
    (49, 100),  # automl
    (50, 31),  # audio
    (52, 98),  # dojo
    (53, 232),  # efficient-frontier
    (54, 236),  # docs-insights
    (57, 237),  # gaia
    (59, 249),  # agent-arena
]
# Proof publishing service URL
PPS_URL = os.getenv(
    "SUBNET_2_PPS_URL",
    "https://pps.inferencelabs.com/",
)
# Testnet PPS URL
TESTNET_PPS_URL = os.getenv(
    "SUBNET_2_PPS_URL",
    "https://cllswjfpzmg67rwythmiiufvtm0gsthd.lambda-url.us-east-1.on.aws/",
)
# EZKL path
LOCAL_EZKL_PATH = os.path.join(os.path.expanduser("~"), ".ezkl", "ezkl")
# GitHub repository URL
REPO_URL = "https://github.com/inference-labs-inc/subnet-2"
# Various time constants in seconds
ONE_SECOND = 1
ONE_MINUTE = 60
FIVE_MINUTES = ONE_MINUTE * 5
ONE_HOUR = ONE_MINUTE * 60
ONE_DAY = ONE_HOUR * 24
ONE_YEAR = ONE_DAY * 365
# Temporary folder for storing proof files
TEMP_FOLDER = "/tmp/subnet-2"
# DSperse slices file name
DSPERSE_SLICES_FILE_NAME = "slices.dsperse"

# Queue size limits
MAX_POW_QUEUE_SIZE = 1024
MAX_EVALUATION_ITEMS = 1024

# Maximum circuit size in GB for competitions
MAX_CIRCUIT_SIZE_GB = 50
# EMA boost factor
EMA_BOOST_FACTOR = 1.2
# EMA enabled flag
EMA_ENABLED = False
