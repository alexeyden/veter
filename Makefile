# Build and install veter, vcat, and vmux. Mirrors what install.sh used
# to do — `make install` builds the three binaries in release mode and
# drops them into $(BINDIR), plus a desktop entry into $(APPDIR).
#
# Override with `make install PREFIX=/usr/local`. Honors $CARGO_TARGET_DIR
# (and `target-dir` set in any cargo config) by asking `cargo metadata`
# for the workspace target directory.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications

PACKAGES := veter vcat vmux
DESKTOP_FILE := $(APPDIR)/veter.desktop

CARGO ?= cargo
INSTALL ?= install

# Resolve the workspace target directory from `cargo metadata` so a
# `target-dir` set in .cargo/config.toml or $CARGO_TARGET_DIR is
# respected. Fall back to $(CURDIR)/target if cargo or python3 isn't
# available.
ifndef TARGET_DIR
TARGET_DIR := $(shell $(CARGO) metadata --no-deps --format-version=1 2>/dev/null | \
    python3 -c 'import sys, json; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null)
ifeq ($(TARGET_DIR),)
TARGET_DIR := $(CURDIR)/target
endif
endif

RELEASE_DIR := $(TARGET_DIR)/release
BINS := $(addprefix $(RELEASE_DIR)/,$(PACKAGES))

.PHONY: all build install uninstall clean help install-desktop

all: install

help:
	@echo "Targets:"
	@echo "  build      cargo build --release for $(PACKAGES)"
	@echo "  install    build and copy binaries into \$$BINDIR + desktop entry"
	@echo "  uninstall  remove installed binaries and desktop entry"
	@echo "  clean      cargo clean"
	@echo
	@echo "Variables (override on the command line):"
	@echo "  PREFIX=$(PREFIX)"
	@echo "  BINDIR=$(BINDIR)"
	@echo "  APPDIR=$(APPDIR)"

build:
	$(CARGO) build --release $(addprefix --package=,$(PACKAGES))

# Each binary depends on the build step. Listing them as separate
# prerequisites of `install` lets make report a clear error if any one
# is missing after the build.
$(BINS): build

install: $(BINS) install-desktop
	@$(INSTALL) -d $(BINDIR)
	@for pkg in $(PACKAGES); do \
	    src="$(RELEASE_DIR)/$$pkg"; \
	    if [ ! -x "$$src" ]; then \
	        echo "error: $$pkg binary not found at $$src" >&2; \
	        exit 1; \
	    fi; \
	    $(INSTALL) -m 0755 "$$src" "$(BINDIR)/$$pkg"; \
	    echo "    $$pkg -> $(BINDIR)/$$pkg"; \
	done
	@case ":$$PATH:" in \
	    *":$(BINDIR):"*) ;; \
	    *) echo "note: $(BINDIR) is not on \$$PATH; add it to your shell rc to use the binaries" ;; \
	esac

# Always (re)generate the desktop entry: its Exec/TryExec embed
# $(BINDIR), so a $(PREFIX) override needs to refresh it even if the
# file already exists from a previous run.
install-desktop:
	@$(INSTALL) -d $(APPDIR)
	@printf '%s\n' \
	    '[Desktop Entry]' \
	    'Type=Application' \
	    'Name=Veter' \
	    'GenericName=Terminal' \
	    'Comment=Veter — VGE/PRT-aware terminal emulator' \
	    'Exec=$(BINDIR)/veter' \
	    'TryExec=$(BINDIR)/veter' \
	    'Icon=utilities-terminal' \
	    'Terminal=false' \
	    'Categories=System;TerminalEmulator;' \
	    'Keywords=shell;prompt;command;commandline;cmd;' \
	    'StartupNotify=true' \
	    > $(DESKTOP_FILE)
	@chmod 0644 $(DESKTOP_FILE)
	@echo "    veter.desktop -> $(DESKTOP_FILE)"
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database "$(APPDIR)" >/dev/null 2>&1 || true; \
	fi

uninstall:
	@for pkg in $(PACKAGES); do \
	    rm -f "$(BINDIR)/$$pkg" && echo "    removed $(BINDIR)/$$pkg"; \
	done
	@rm -f "$(DESKTOP_FILE)" && echo "    removed $(DESKTOP_FILE)"
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database "$(APPDIR)" >/dev/null 2>&1 || true; \
	fi

clean:
	$(CARGO) clean
