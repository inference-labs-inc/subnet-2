#!/usr/bin/env bash

set -eo pipefail


NODE_VERSION="20"
INSTALL_PATH="./subnet-2"
if git rev-parse --is-inside-work-tree &>/dev/null; then
    INSTALL_PATH="."
fi


BREW_PACKAGES=(
    "node@${NODE_VERSION}"
    "jq"
    "aria2"
    "pkg-config"
    "certifi"
    "ca-certificates"
    "openssl"
    "pipx"
    "open-mpi"
)

APT_PACKAGES=(
    "jq"
    "aria2"
    "pkg-config"
    "libssl-dev"
    "openssl"
    "pipx"
    "libopenmpi-dev"
    "openmpi-bin"
)

case "$(uname)" in
    "Darwin")
        if ! command -v brew &>/dev/null; then
            /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
            eval "$(/opt/homebrew/bin/brew shellenv)"
        fi

        echo "Installing brew packages..."
        brew update
        for pkg in "${BREW_PACKAGES[@]}"; do
            brew install "$pkg" || brew upgrade "$pkg"
        done

        brew link --force "node@${NODE_VERSION}"

        npm config set cafile /etc/ssl/cert.pem
        ;;

    "Linux")
        echo "Installing apt packages..."
        sudo apt update
        sudo apt install -y "${APT_PACKAGES[@]}"

        curl -fsSL "https://deb.nodesource.com/setup_${NODE_VERSION}.x" | sudo -E bash -
        sudo apt install -y nodejs
        ;;

    *)
        echo "Unsupported OS"
        exit 1
        ;;
esac

echo "Checking for SnarkJS..."
local_snarkjs_dir="${HOME}/.snarkjs"
local_snarkjs_path="${local_snarkjs_dir}/node_modules/.bin/snarkjs"
if ! command -v "${local_snarkjs_path} r1cs info --help" >/dev/null 2>&1; then
    echo "SnarkJS 0.7.4 not found in local directory. Installing..."
    mkdir -p "${local_snarkjs_dir}"
    npm install --prefix "${local_snarkjs_dir}" snarkjs@0.7.4
    echo "SnarkJS has been installed in the local directory."
fi

echo "Installing pm2..."
sudo npm install -g pm2

pipx ensurepath
export PATH="$HOME/.local/bin:$PATH"

echo "Installing uv..."
pipx install uv

echo "Installing btcli..."
"$HOME/.local/bin/uv" tool install --python 3.12 bittensor-cli
"$HOME/.local/bin/uv" tool install --python 3.12 JSTprove

if [[ ! -d ${INSTALL_PATH} ]]; then
    echo "Cloning subnet-2 repository..."
    if ! git clone https://github.com/inference-labs-inc/subnet-2.git "${INSTALL_PATH}"; then
        echo "Failed to clone repository. Check your internet connection and try again."
        exit 1
    fi
fi

cd "${INSTALL_PATH}" || {
    echo "Failed to change to ${INSTALL_PATH} directory"
    exit 1
}

"$HOME/.local/bin/uv" sync --frozen --no-dev

echo "
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@@@@@@@@@@@%#%@@@@@@@@@@@@@@@@@@
@@@@@@@@*.......*@@@@@@@@@@@@@@@
@@@@@@@+.........+@@@@@@@@@@@@@@
@@@@@@@:.....-#%%+..:=%@@@@@@@@@
@@@@@@@*...:#@@@@=.....=@@@@@@@@
@@@@@@@@#-.#@@@#-.......+@@@@@@@
@@@@@@@@@@*.-:..........:@@@@@@@
@@@@@@@@@@%.............-@@@@@@@
@@@@@@@@@@@*...........:%@@@@@@@
@@@@@@@@@@@@@-........*@@@@@@@@@
@@@@@@@@@@@@@@@%###%%@@@@@@@@@@@
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
███████╗██╗   ██╗██████╗ ███╗   ██╗███████╗████████╗ 2
██╔════╝██║   ██║██╔══██╗████╗  ██║██╔════╝╚══██╔══╝
███████╗██║   ██║██████╔╝██╔██╗ ██║█████╗     ██║
╚════██║██║   ██║██╔══██╗██║╚██╗██║██╔══╝     ██║
███████║╚██████╔╝██████╔╝██║ ╚████║███████╗   ██║
╚══════╝ ╚═════╝ ╚═════╝ ╚═╝  ╚═══╝╚══════╝   ╚═╝
"
echo "🥩 Setup complete! 🥩"
echo "Next steps:"
echo "1. Re-login for PATH changes to take effect, or run 'source ~/.bashrc' or 'source ~/.zshrc'"
echo "2. Check ${INSTALL_PATH}/docs/shared_setup_steps.md to setup your wallet and register on the subnet"
echo "3. cd ${INSTALL_PATH}"
echo "4. make <pm2-miner|pm2-validator> WALLET_NAME=<your_wallet_name> WALLET_HOTKEY=<your_wallet_hotkey>"
