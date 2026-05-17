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

.PHONY: build codesign install uninstall notarize clean

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

clean:
	$(CARGO) clean
