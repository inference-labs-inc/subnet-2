OS := $(shell uname)
NETUID ?= 2
WALLET_NAME ?= default
WALLET_HOTKEY ?= default
WALLET_PATH ?= $(HOME)/.bittensor
ifeq ($(OS),Darwin)
    PUID ?= $(shell stat -f %u $(WALLET_PATH))
else
    PUID ?= $(shell stat -c %u $(WALLET_PATH))
endif
MINER_PORT ?= 8091
VALIDATOR_PORT ?= 8443

.PHONY: build stop clean miner-logs validator-logs miner validator test-miner test-validator

build:
	docker build -t subnet-2 -f Dockerfile .

stop:
	docker stop subnet-2-miner || true
	docker stop subnet-2-validator || true

clean:
	docker stop subnet-2-miner || true
	docker stop subnet-2-validator || true
	docker rm subnet-2-miner || true
	docker rm subnet-2-validator || true
	docker image rm subnet-2 || true
	docker image prune -f

miner-logs:
	docker logs -f subnet-2-miner

validator-logs:
	docker logs -f subnet-2-validator

check-extra-args:
	@if [ -n "$(ARGS)" ]; then \
		echo "Extra arguments: $(ARGS)"; \
	fi

miner: check-extra-args
	@echo "Using wallet path: $(WALLET_PATH)"
	@echo "Setting PUID to $(PUID)"
	docker stop subnet-2-miner || true
	docker rm subnet-2-miner || true
	docker run \
		--detach \
		--name subnet-2-miner \
		-p $(MINER_PORT):8091 \
		-v $(WALLET_PATH):/home/ubuntu/.bittensor \
		-e PUID=$(PUID) \
		subnet-2 miner.py \
		--wallet.name $(WALLET_NAME) \
		--wallet.hotkey $(WALLET_HOTKEY) \
		--netuid $(NETUID) \
		$(ARGS)

validator: check-extra-args
	@echo "Using wallet path: $(WALLET_PATH)"
	@echo "Setting PUID to $(PUID)"
	docker stop subnet-2-validator || true
	docker rm subnet-2-validator || true
	docker run \
		--detach \
		--name subnet-2-validator \
		-p $(VALIDATOR_PORT):8443 \
		-v $(WALLET_PATH):/home/ubuntu/.bittensor \
		-e PUID=$(PUID) \
		subnet-2 validator.py \
		--wallet.name $(WALLET_NAME) \
		--wallet.hotkey $(WALLET_HOTKEY) \
		--netuid $(NETUID) \
		$(ARGS)

test-miner: check-extra-args
	@echo "Using wallet path: $(WALLET_PATH)"
	@echo "Setting PUID to $(PUID)"
	docker stop subnet-2-miner || true
	docker rm subnet-2-miner || true
	docker run \
		--detach \
		--name subnet-2-miner \
		-p $(MINER_PORT):8091 \
		-v $(WALLET_PATH):/home/ubuntu/.bittensor \
		-e PUID=$(PUID) \
		subnet-2 miner.py \
		--wallet.name $(WALLET_NAME) \
		--wallet.hotkey $(WALLET_HOTKEY) \
		--netuid 118 \
		--subtensor.network test \
		--disable-blacklist \
		$(ARGS)

test-validator: check-extra-args
	@echo "Using wallet path: $(WALLET_PATH)"
	@echo "Setting PUID to $(PUID)"
	docker stop subnet-2-validator || true
	docker rm subnet-2-validator || true
	docker run \
		--detach \
		--name subnet-2-validator \
		-p $(VALIDATOR_PORT):8443 \
		-v $(WALLET_PATH):/home/ubuntu/.bittensor \
		-e PUID=$(PUID) \
		subnet-2 validator.py \
		--wallet.name $(WALLET_NAME) \
		--wallet.hotkey $(WALLET_HOTKEY) \
		--netuid 118 \
		--subtensor.network test \
		$(ARGS)

local-miner: check-extra-args
	@echo "Starting local miner on staging"
	cd neurons; \
	../.venv/bin/python miner.py \
	--localnet \
	--no-auto-update \
	$(ARGS)

local-validator: check-extra-args
	@echo "Starting local validator on staging"
	cd neurons; \
	../.venv/bin/python validator.py \
	--localnet \
	--no-auto-update \
	$(ARGS)

debug-local-miner: check-extra-args
	@echo "Starting local miner on staging with remote debugger"
	.venv/bin/python -m debugpy --listen localhost:5678 --wait-for-client neurons/miner.py \
	--localnet \
	--no-auto-update \
	$(ARGS)

debug-local-validator: check-extra-args
	@echo "Starting local validator on staging with remote debugger"
	.venv/bin/python -m debugpy --listen localhost:5678 --wait-for-client neurons/validator.py \
	--localnet \
	--no-auto-update \
	$(ARGS)

debug-test-miner: check-extra-args
	@echo "Starting miner on testnet with remote debugger"
	.venv/bin/python -m debugpy --listen localhost:5678 --wait-for-client neurons/miner.py \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid 118 \
	--subtensor.network test \
	--disable-blacklist \
	$(ARGS)

debug-test-validator: check-extra-args
	@echo "Starting validator on testnet with remote debugger"
	.venv/bin/python -m debugpy --listen localhost:5678 --wait-for-client neurons/validator.py \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid 118 \
	--subtensor.network test \
	$(ARGS)

debug-finney-validator: check-extra-args
	@echo "Starting validator on mainnet with remote debugger"
	.venv/bin/python -m debugpy --listen localhost:5678 --wait-for-client neurons/validator.py \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid $(NETUID) \
	$(ARGS)

pm2-setup:
	./setup.sh

pm2-stop:
	pm2 stop subnet-2-miner || true
	pm2 stop subnet-2-validator || true

pm2-miner: check-extra-args
	uv sync --frozen --no-dev
	cd neurons; \
	pm2 start miner.py --name subnet-2-miner --interpreter ../.venv/bin/python --kill-timeout 3000 -- \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid $(NETUID) \
	$(ARGS)

pm2-validator: check-extra-args
	uv sync --frozen --no-dev
	cd neurons; \
	pm2 start validator.py --name subnet-2-validator --interpreter ../.venv/bin/python --kill-timeout 3000 -- \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid $(NETUID) \
	$(ARGS)

pm2-test-miner: check-extra-args
	uv sync --frozen --no-dev
	cd neurons; \
	pm2 start miner.py --name subnet-2-miner --interpreter ../.venv/bin/python --kill-timeout 3000 -- \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid 118 \
	--subtensor.network test \
	--disable-blacklist \
	$(ARGS)

pm2-test-validator: check-extra-args
	uv sync --frozen --no-dev
	cd neurons; \
	pm2 start validator.py --name subnet-2-validator --interpreter ../.venv/bin/python --kill-timeout 3000 -- \
	--wallet.path $(WALLET_PATH)/wallets \
	--wallet.name $(WALLET_NAME) \
	--wallet.hotkey $(WALLET_HOTKEY) \
	--netuid 118 \
	--subtensor.network test \
	$(ARGS)
