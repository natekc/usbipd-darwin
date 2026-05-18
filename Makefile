# usbipd-darwin — release / install Makefile
#
# `make build`          — cargo build --release
# `make codesign`       — sign the release binary with hardened runtime
#                         (set SIGN_ID, e.g. SIGN_ID="Developer ID Application: Foo")
# `make install`        — install signed binary + sample launchd plist
# `make uninstall`      — remove launchd unit + binary
# `make notarize`       — submit the binary to Apple's notary service
#                         (set APPLE_ID, TEAM_ID, NOTARY_PROFILE)

PREFIX        ?= /usr/local
BIN_DIR       ?= $(PREFIX)/bin
LAUNCHD_DIR   ?= /Library/LaunchDaemons
LAUNCHD_LABEL ?= io.usbipd.daemon

CARGO         ?= cargo
TARGET_DIR    ?= target
RELEASE_BIN   := $(TARGET_DIR)/release/usbipd
ENTITLEMENTS  := dist/usbipd.entitlements
PLIST         := dist/launchd/$(LAUNCHD_LABEL).plist

SIGN_ID       ?= -            # `-` means ad-hoc; override for distribution

.PHONY: build codesign install uninstall notarize docs clean

build:
	$(CARGO) build --release

# Hardened runtime is required for notarization and is harmless ad-hoc.
codesign: build
	codesign --force --options runtime --timestamp \
		--sign "$(SIGN_ID)" \
		--entitlements $(ENTITLEMENTS) \
		$(RELEASE_BIN)
	codesign -dv --verbose=2 $(RELEASE_BIN)

install: codesign
	install -d $(BIN_DIR)
	install -m 0755 $(RELEASE_BIN) $(BIN_DIR)/usbipd
	install -d $(LAUNCHD_DIR)
	install -m 0644 $(PLIST) $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist
	@echo
	@echo "Installed. Edit $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist to set"
	@echo "your access policy, then load with:"
	@echo "  sudo launchctl bootstrap system $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist"

uninstall:
	-sudo launchctl bootout system/$(LAUNCHD_LABEL) 2>/dev/null
	-rm -f $(LAUNCHD_DIR)/$(LAUNCHD_LABEL).plist
	-rm -f $(BIN_DIR)/usbipd

# Requires `xcrun notarytool store-credentials <NOTARY_PROFILE>` to have
# been run interactively beforehand.
notarize: codesign
	@test -n "$(NOTARY_PROFILE)" || (echo "set NOTARY_PROFILE" && exit 1)
	ditto -c -k --keepParent $(RELEASE_BIN) $(TARGET_DIR)/usbipd.zip
	xcrun notarytool submit $(TARGET_DIR)/usbipd.zip \
		--keychain-profile "$(NOTARY_PROFILE)" \
		--wait

# Regenerate docs/cli.md by capturing `--help` for every subcommand.
# Maintainers should `make docs && git add docs/cli.md` whenever they
# touch CLI surface.
docs: build
	@mkdir -p docs
	@{ \
		echo '# usbipd CLI reference'; \
		echo; \
		echo '_Auto-generated from `usbipd --help`. Regenerate with `make docs`._'; \
		echo; \
		echo '## `usbipd`'; \
		echo; \
		echo '```'; \
		$(RELEASE_BIN) --help; \
		echo '```'; \
		for sub in list daemon events sudoers release-capture; do \
			echo; \
			echo "## \`usbipd $$sub\`"; \
			echo; \
			echo '```'; \
			$(RELEASE_BIN) $$sub --help 2>&1; \
			echo '```'; \
		done; \
	} > docs/cli.md
	@echo "Wrote docs/cli.md"

clean:
	$(CARGO) clean
