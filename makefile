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

.PHONY: setup build cargo-build check clippy test fmt fmt-check stop clean miner-logs validator-logs miner validator test-miner test-validator check-extra-args pm2-miner pm2-validator pm2-stop

setup:
	git config core.hooksPath .githooks

build:
	docker build -t subnet-2 -f Dockerfile .

cargo-build:
	cargo build --release --locked --bin sn2-validator --bin sn2-miner

check:
	cargo check --workspace

clippy:
	cargo clippy --workspace -- -D warnings

test:
	cargo test --workspace

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

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
		-v $(WALLET_PATH):/home/subnet2/.bittensor \
		-e PUID=$(PUID) \
		-e HOME=/home/subnet2 \
		subnet-2 sn2-miner \
		--wallet-name $(WALLET_NAME) \
		--wallet-hotkey $(WALLET_HOTKEY) \
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
		-v $(WALLET_PATH):/home/subnet2/.bittensor \
		-e PUID=$(PUID) \
		-e HOME=/home/subnet2 \
		subnet-2 sn2-validator \
		--wallet-name $(WALLET_NAME) \
		--wallet-hotkey $(WALLET_HOTKEY) \
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
		-v $(WALLET_PATH):/home/subnet2/.bittensor \
		-e PUID=$(PUID) \
		-e HOME=/home/subnet2 \
		subnet-2 sn2-miner \
		--wallet-name $(WALLET_NAME) \
		--wallet-hotkey $(WALLET_HOTKEY) \
		--netuid 118 \
		--network test \
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
		-v $(WALLET_PATH):/home/subnet2/.bittensor \
		-e PUID=$(PUID) \
		-e HOME=/home/subnet2 \
		subnet-2 sn2-validator \
		--wallet-name $(WALLET_NAME) \
		--wallet-hotkey $(WALLET_HOTKEY) \
		--netuid 118 \
		--network test \
		$(ARGS)

pm2-miner: cargo-build check-extra-args
	pm2 delete subnet-2-miner || true
	pm2 start target/release/sn2-miner --name subnet-2-miner --kill-timeout 3000 -- \
	--wallet-path $(WALLET_PATH)/wallets \
	--wallet-name $(WALLET_NAME) \
	--wallet-hotkey $(WALLET_HOTKEY) \
	--netuid $(NETUID) \
	$(ARGS)

pm2-validator: cargo-build check-extra-args
	pm2 delete subnet-2-validator || true
	pm2 start target/release/sn2-validator --name subnet-2-validator --kill-timeout 3000 -- \
	--wallet-path $(WALLET_PATH)/wallets \
	--wallet-name $(WALLET_NAME) \
	--wallet-hotkey $(WALLET_HOTKEY) \
	--netuid $(NETUID) \
	$(ARGS)

pm2-stop:
	pm2 stop subnet-2-miner || true
	pm2 stop subnet-2-validator || true
