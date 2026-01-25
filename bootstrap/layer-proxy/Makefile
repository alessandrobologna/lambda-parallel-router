# Makefile for building the Mode A Runtime API proxy extension layer.
#
# The SAM Makefile build method runs `make build-<LogicalId>` with ARTIFACTS_DIR set.

EXTENSION_NAME := lpr-runtime-api-proxy
MANIFEST_PATH := Cargo.toml
EXEC_WRAPPER := layer/lpr/exec-wrapper.sh

define build-extension-layer
	@echo "Building $(EXTENSION_NAME) for $(1)"
	@cargo lambda build --release --extension --$(2) --manifest-path "$(MANIFEST_PATH)"
	@echo "Copying layer files to artifacts directory"
	@mkdir -p "$(ARTIFACTS_DIR)/extensions" "$(ARTIFACTS_DIR)/lpr"
	@cp "$(shell cargo metadata --format-version=1 --manifest-path "$(MANIFEST_PATH)" | jq -r '.target_directory')/lambda/extensions/$(EXTENSION_NAME)" "$(ARTIFACTS_DIR)/extensions/$(EXTENSION_NAME)"
	@cp "$(EXEC_WRAPPER)" "$(ARTIFACTS_DIR)/lpr/exec-wrapper.sh"
	@chmod +x "$(ARTIFACTS_DIR)/extensions/$(EXTENSION_NAME)" "$(ARTIFACTS_DIR)/lpr/exec-wrapper.sh"
endef

build-LprRuntimeApiProxyLayerArm64:
	$(call build-extension-layer,arm64,arm64)

build-LprRuntimeApiProxyLayerAmd64:
	$(call build-extension-layer,amd64,x86-64)
