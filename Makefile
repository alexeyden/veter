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
ICONROOT ?= $(PREFIX)/share/icons/hicolor

PACKAGES := veter vcat vmux vsend vrecv veterd vssh
DESKTOP_FILE := $(APPDIR)/veter.desktop
ICON_SVG_SRC := $(CURDIR)/assets/veter.svg
ICON_SVG_DST := $(ICONROOT)/scalable/apps/veter.svg

# Raster sizes installed for desktop menus that don't pick up the
# scalable SVG (KDE Plasma's menu cache, GTK older versions, etc).
ICON_PNG_SIZES := 16 32 48 64 128 256

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

.PHONY: all build install uninstall clean help install-desktop install-icon \
        dist-aarch64-build dist-aarch64-tarxz dist-aarch64-deb dist-clean \
        dist-aarch64-manifest install-dist-maybe install-dist-share \
        install-remote-aarch64

all: install

help:
	@echo "Targets:"
	@echo "  build               cargo build --release for $(PACKAGES)"
	@echo "  install             build and copy binaries into \$$BINDIR + desktop entry"
	@echo "  uninstall           remove installed binaries and desktop entry"
	@echo "  clean               cargo clean"
	@echo
	@echo "  dist-aarch64-build  cross-compile vmux/vcat/vsend/vrecv/veterd"
	@echo "                      for aarch64-unknown-linux-musl (static, rust-lld)"
	@echo "  dist-aarch64-tarxz  bundle the above into a .tar.xz under dist/"
	@echo "  dist-aarch64-manifest  write a sha256-stamped manifest beside the tarball"
	@echo "  dist-aarch64-deb    bundle the above into veter-tools_<v>_arm64.deb"
	@echo "  install-dist-share  stage tarball + manifest under \$$PREFIX/share/veter/dist/"
	@echo "                      (vssh reads them from there to deploy remotely)"
	@echo "  dist-clean          rm -rf dist/"
	@echo
	@echo "  install-remote-aarch64"
	@echo "                      cross-compile and scp+install the aarch64-musl"
	@echo "                      binaries into REMOTE_BINDIR on REMOTE (over ssh)"
	@echo
	@echo "Variables (override on the command line):"
	@echo "  PREFIX=$(PREFIX)"
	@echo "  BINDIR=$(BINDIR)"
	@echo "  APPDIR=$(APPDIR)"
	@echo "  DIST_VERSION=$(DIST_VERSION)"
	@echo "  REMOTE=$(REMOTE)  (e.g. ha@home-assistant.local — required for install-remote-aarch64)"
	@echo "  REMOTE_BINDIR=$(REMOTE_BINDIR)"

build:
	$(CARGO) build --release $(addprefix --package=,$(PACKAGES))

# Each binary depends on the build step. Listing them as separate
# prerequisites of `install` lets make report a clear error if any one
# is missing after the build.
$(BINS): build

install: $(BINS) install-desktop install-icon install-dist-maybe
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
	    'Icon=veter' \
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

# Install the SVG into the freedesktop hicolor scalable apps path plus
# raster PNGs at standard sizes; the .desktop file's `Icon=veter` then
# resolves on any compliant desktop env. Refresh the icon-theme cache
# at the end so KDE Plasma's menu picks the new icon up immediately.
install-icon:
	@$(INSTALL) -d $(ICONROOT)/scalable/apps
	@$(INSTALL) -m 0644 $(ICON_SVG_SRC) $(ICON_SVG_DST)
	@echo "    veter.svg -> $(ICON_SVG_DST)"
	@for sz in $(ICON_PNG_SIZES); do \
	    src="$(CURDIR)/assets/icons/$${sz}x$${sz}/veter.png"; \
	    dst="$(ICONROOT)/$${sz}x$${sz}/apps/veter.png"; \
	    $(INSTALL) -d "$(ICONROOT)/$${sz}x$${sz}/apps"; \
	    $(INSTALL) -m 0644 "$$src" "$$dst"; \
	    echo "    veter.png ($${sz}px) -> $$dst"; \
	done
	@if command -v gtk-update-icon-cache >/dev/null 2>&1; then \
	    gtk-update-icon-cache -q -f -t "$(ICONROOT)" >/dev/null 2>&1 || true; \
	fi

uninstall:
	@for pkg in $(PACKAGES); do \
	    rm -f "$(BINDIR)/$$pkg" && echo "    removed $(BINDIR)/$$pkg"; \
	done
	@if [ -d "$(PREFIX)/share/veter/dist" ]; then \
	    rm -rf "$(PREFIX)/share/veter/dist" && \
	    echo "    removed $(PREFIX)/share/veter/dist"; \
	fi
	@rm -f "$(DESKTOP_FILE)" && echo "    removed $(DESKTOP_FILE)"
	@rm -f "$(ICON_SVG_DST)" && echo "    removed $(ICON_SVG_DST)"
	@for sz in $(ICON_PNG_SIZES); do \
	    rm -f "$(ICONROOT)/$${sz}x$${sz}/apps/veter.png" && \
	    echo "    removed $(ICONROOT)/$${sz}x$${sz}/apps/veter.png"; \
	done
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database "$(APPDIR)" >/dev/null 2>&1 || true; \
	fi
	@if command -v gtk-update-icon-cache >/dev/null 2>&1; then \
	    gtk-update-icon-cache -q -f -t "$(ICONROOT)" >/dev/null 2>&1 || true; \
	fi

clean:
	$(CARGO) clean

# ---- aarch64-musl static distribution of client-side tools ----------
#
# Cross-builds vmux, vcat, vsend, vrecv for aarch64-unknown-linux-musl
# using rust-lld (which ships with rustup-installed rustc, so no host
# toolchain prereq beyond `rustup target add`). The resulting binaries
# are fully static — no dynamic loader, no libc dependency on the
# target system — and ride into either a .tar.xz or a .deb.

DIST_TOOLS := vmux vcat vsend vrecv veterd
DIST_VERSION ?= 0.1.6
DIST_ARCH := aarch64-unknown-linux-musl

# `install-remote-aarch64` knobs. `REMOTE` is required — it's whatever
# ssh(1) would accept (`user@host`, `host`, or a `Host` alias from
# `~/.ssh/config`). `REMOTE_BINDIR` is where the binaries land on the
# remote; ~/.local/bin keeps things user-scoped, no sudo needed. Both
# variables can be set on the make CLI or in the environment.
REMOTE ?=
REMOTE_BINDIR ?= ~/.local/bin
DIST_DEB_ARCH := arm64
DIST_BINDIR := $(TARGET_DIR)/$(DIST_ARCH)/release
DIST_DIR := $(CURDIR)/dist
DIST_DEB_STAGING := $(DIST_DIR)/veter-tools_$(DIST_VERSION)_$(DIST_DEB_ARCH)
DIST_DEB_FILE := $(DIST_DIR)/veter-tools_$(DIST_VERSION)_$(DIST_DEB_ARCH).deb
DIST_TARXZ_STAGING := $(DIST_DIR)/staging-tarxz
DIST_TARXZ_FILE := $(DIST_DIR)/veter-tools-$(DIST_VERSION)-$(DIST_ARCH).tar.xz
DIST_MANIFEST_FILE := $(DIST_DIR)/manifest-$(DIST_ARCH).json
DIST_MAINTAINER := Alexey Denisov <rtgbnm@gmail.com>

# vssh reads the staged tarball + manifest from this layout when
# installing veter-tools on a remote host. `install-dist-maybe` only
# stages them if the aarch64-musl rust-std is available; otherwise
# vssh degrades to a thin ssh wrapper.
SHARE_DIST_DIR := $(PREFIX)/share/veter/dist/$(DIST_ARCH)
SHARE_DIST_TARBALL := $(SHARE_DIST_DIR)/veter-tools.tar.xz
SHARE_DIST_MANIFEST := $(SHARE_DIST_DIR)/manifest.json

dist-aarch64-build:
	@# Linker for this target is configured in `.cargo/config.toml`:
	@# `linker = "rust-lld"`. rust-lld ships inside the rustup
	@# toolchain so no host cross-gcc is required.
	@rustup target list --installed 2>/dev/null | grep -qx '$(DIST_ARCH)' || { \
	    echo "Installing rust-std for $(DIST_ARCH)..."; \
	    rustup target add $(DIST_ARCH); \
	}
	$(CARGO) build --release --target $(DIST_ARCH) \
	    $(addprefix --package=,$(DIST_TOOLS))
	@for t in $(DIST_TOOLS); do \
	    src="$(DIST_BINDIR)/$$t"; \
	    if [ ! -x "$$src" ]; then \
	        echo "error: $$t binary not found at $$src" >&2; \
	        exit 1; \
	    fi; \
	done

dist-aarch64-tarxz: dist-aarch64-build
	@$(INSTALL) -d $(DIST_DIR)
	@rm -rf $(DIST_TARXZ_STAGING)
	@$(INSTALL) -d $(DIST_TARXZ_STAGING)/veter-tools-$(DIST_VERSION)
	@for t in $(DIST_TOOLS); do \
	    $(INSTALL) -m 0755 $(DIST_BINDIR)/$$t \
	        $(DIST_TARXZ_STAGING)/veter-tools-$(DIST_VERSION)/$$t; \
	done
	@printf '%s\n' \
	    'veter-tools $(DIST_VERSION) — $(DIST_ARCH)' \
	    '' \
	    'Remote-side tools for the Veter terminal emulator. Statically' \
	    'linked against musl libc; drop the binaries anywhere on $$PATH on' \
	    'an aarch64 Linux host and they will work without any runtime' \
	    'dependencies.' \
	    '' \
	    'Tools:' \
	    '  vmux    terminal multiplexer (PRT + VGE)' \
	    '  vcat    display images inline (VGE)' \
	    '  vsend   upload local files (VFT)' \
	    '  vrecv   download remote files (VFT)' \
	    '  veterd  persistent session daemon (doc/session-manager.md)' \
	    > $(DIST_TARXZ_STAGING)/veter-tools-$(DIST_VERSION)/README
	@tar -cJf $(DIST_TARXZ_FILE) \
	    -C $(DIST_TARXZ_STAGING) veter-tools-$(DIST_VERSION)
	@rm -rf $(DIST_TARXZ_STAGING)
	@echo "    veter-tools tarball -> $(DIST_TARXZ_FILE)"
	@ls -l $(DIST_TARXZ_FILE)

# Manifest is what vssh reads to decide whether to upload to a remote.
# Carrying the sha256 of the tarball (not just the version string)
# lets us treat any content change as a reason to push, including
# rebuilds where the version number didn't move.
dist-aarch64-manifest: dist-aarch64-tarxz
	@sha=$$(sha256sum $(DIST_TARXZ_FILE) | cut -d' ' -f1); \
	tools=$$(printf '"%s",' $(DIST_TOOLS) | sed 's/,$$//'); \
	printf '{"version":"%s","arch":"%s","sha256":"%s","tools":[%s]}\n' \
	    "$(DIST_VERSION)" "$(DIST_ARCH)" "$$sha" "$$tools" \
	    > $(DIST_MANIFEST_FILE)
	@echo "    manifest -> $(DIST_MANIFEST_FILE)"

install-dist-share: dist-aarch64-manifest
	@$(INSTALL) -d $(SHARE_DIST_DIR)
	@$(INSTALL) -m 0644 $(DIST_TARXZ_FILE) $(SHARE_DIST_TARBALL)
	@$(INSTALL) -m 0644 $(DIST_MANIFEST_FILE) $(SHARE_DIST_MANIFEST)
	@echo "    dist tarball -> $(SHARE_DIST_TARBALL)"
	@echo "    dist manifest -> $(SHARE_DIST_MANIFEST)"

# Run as a dep of `install`. Stages the dist tarball under
# $PREFIX/share/ when the rustup target for aarch64-musl is present,
# otherwise drops a single note line and proceeds. The recursive
# $(MAKE) is deliberate: it lets `install-dist-share` pull in
# dist-aarch64-tarxz → dist-aarch64-build only when the target is
# actually installed, keeping a vanilla `make install` toolchain-free.
install-dist-maybe:
	@if rustup target list --installed 2>/dev/null | grep -qx '$(DIST_ARCH)'; then \
	    $(MAKE) --no-print-directory install-dist-share; \
	else \
	    echo "note: $(DIST_ARCH) rust-std not installed; vssh will run without bundled remote tools."; \
	    echo "      run 'rustup target add $(DIST_ARCH)' to enable auto-deploy on ssh."; \
	fi

dist-aarch64-deb: dist-aarch64-build
	@command -v dpkg-deb >/dev/null 2>&1 || { \
	    echo "error: dpkg-deb required; install the 'dpkg' package" >&2; \
	    exit 1; \
	}
	@$(INSTALL) -d $(DIST_DIR)
	@rm -rf $(DIST_DEB_STAGING)
	@$(INSTALL) -d $(DIST_DEB_STAGING)/DEBIAN $(DIST_DEB_STAGING)/usr/bin
	@for t in $(DIST_TOOLS); do \
	    $(INSTALL) -m 0755 $(DIST_BINDIR)/$$t $(DIST_DEB_STAGING)/usr/bin/$$t; \
	done
	@printf '%s\n' \
	    'Package: veter-tools' \
	    'Version: $(DIST_VERSION)' \
	    'Section: utils' \
	    'Priority: optional' \
	    'Architecture: $(DIST_DEB_ARCH)' \
	    'Maintainer: $(DIST_MAINTAINER)' \
	    'Description: Remote-side tools for the Veter terminal emulator' \
	    ' Statically-linked aarch64 binaries for vmux, vcat, vsend, vrecv,' \
	    ' and veterd. The first four talk PRT/VGE/VFT to a Veter-aware' \
	    ' terminal (or to vmux running inside one); veterd is a persistent' \
	    ' session daemon that owns inner PTYs across renderer attach/detach' \
	    ' cycles (see doc/session-manager.md). All binaries have no runtime' \
	    ' dependencies on the target system.' \
	    > $(DIST_DEB_STAGING)/DEBIAN/control
	@dpkg-deb --root-owner-group --build $(DIST_DEB_STAGING) $(DIST_DEB_FILE) >/dev/null
	@rm -rf $(DIST_DEB_STAGING)
	@echo "    veter-tools deb -> $(DIST_DEB_FILE)"
	@ls -l $(DIST_DEB_FILE)

# One-shot deploy to a remote aarch64 host. Streams the freshly-built
# release binaries through a single ssh connection (no intermediate
# tarball lands on disk), then mkdirs REMOTE_BINDIR and untars there.
# Permissions are preserved by tar(1); the binaries land 0755.
#
# Usage:
#   make install-remote-aarch64 REMOTE=ha@home-assistant.local
#   make install-remote-aarch64 REMOTE=ha@home-assistant.local REMOTE_BINDIR=/opt/veter/bin
#   REMOTE=ha@home-assistant.local make install-remote-aarch64
install-remote-aarch64: dist-aarch64-build
	@if [ -z "$(REMOTE)" ]; then \
	    echo "error: REMOTE not set" >&2; \
	    echo "  example: make install-remote-aarch64 REMOTE=ha@home-assistant.local" >&2; \
	    exit 1; \
	fi
	@echo "    veter-tools -> $(REMOTE):$(REMOTE_BINDIR)/"
	@tar -cf - -C $(DIST_BINDIR) $(DIST_TOOLS) \
	    | ssh $(REMOTE) "mkdir -p $(REMOTE_BINDIR) && tar -xpf - -C $(REMOTE_BINDIR)"
	@for t in $(DIST_TOOLS); do echo "    $$t -> $(REMOTE):$(REMOTE_BINDIR)/$$t"; done

dist-clean:
	@rm -rf $(DIST_DIR)
	@echo "    removed $(DIST_DIR)"
