.DEFAULT_GOAL := build

PREFIX ?= $(HOME)/.local
CARGO ?= cargo
TARGET_DIR ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
CRON_SCHEDULE ?= */5 * * * *

.PHONY: build run install uninstall install-service uninstall-service

build:
	"$(CARGO)" build --release

run:
	"$(CARGO)" run -- sync

install: build
	install -d "$(PREFIX)/bin"
	install -m 755 "$(TARGET_DIR)/release/memos" "$(PREFIX)/bin/memos"

uninstall: uninstall-service
	rm -f "$(PREFIX)/bin/memos"

install-service: install
	"$(PREFIX)/bin/memos" service install --schedule '$(CRON_SCHEDULE)'

uninstall-service:
	@if [ -x "$(PREFIX)/bin/memos" ]; then \
		"$(PREFIX)/bin/memos" service uninstall; \
	else \
		"$(CARGO)" run -- service uninstall; \
	fi
