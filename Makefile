.PHONY: install install-ghagent check

# Optional features layered on top of the default feature set. Deliberately
# not `--all-features`: `skills-embed` pulls in `ort-sys`, which needs a
# prebuilt ONNX Runtime that CI and many hosts (including some WSL setups) do
# not have. Override on the command line, e.g. `make install FEATURES=skills`.
FEATURES ?= hooks,lsp,skills,multimodal,pdf,advisor

# Build and reinstall an debug-profile mini-agent from this
# checkout with the default features plus $(FEATURES).
install: install-ghagent
	cargo install --locked --path . --debug --features "$(FEATURES)" --force

# Install the ghagent-mcp companion server (push_branch / wait_for_checks for
# sandboxed agents). Python 3 stdlib only. Never touches ~/.config/ghagent or
# the mini-agent config; see tools/ghagent-mcp/README.md.
PREFIX ?= $(HOME)/.local
install-ghagent:
	install -d "$(PREFIX)/share/ghagent-mcp" "$(PREFIX)/bin"
	install -m 644 tools/ghagent-mcp/ghagent_mcp.py tools/ghagent-mcp/README.md "$(PREFIX)/share/ghagent-mcp/"
	install -m 755 tools/ghagent-mcp/ghagent-mcp tools/ghagent-mcp/ghagent-save-token "$(PREFIX)/bin/"

# Each check runs at most once per unchanged working tree (scripts/check_once.py).
check:
	python3 scripts/check_once.py fmt -- cargo fmt --check
	python3 scripts/check_once.py clippy -- cargo clippy --locked --all-targets -- -D warnings
	python3 scripts/check_once.py test -- cargo test --locked
