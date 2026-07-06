# Build and install veter, vcat, vplay, and vmux. Mirrors what
# install.sh used to do — `make install` builds the binaries in release
# mode and drops them into $(BINDIR), plus a desktop entry into $(APPDIR).
#
# Override with `make install PREFIX=/usr/local`. Honors $CARGO_TARGET_DIR
# (and `target-dir` set in any cargo config) by asking `cargo metadata`
# for the workspace target directory.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications
ICONROOT ?= $(PREFIX)/share/icons/hicolor

PACKAGES := veter vcat vplay vmux vsend vrecv vsd vssh
DESKTOP_FILE := $(APPDIR)/veter.desktop
ICON_SVG_SRC := $(CURDIR)/assets/veter.svg
ICON_SVG_DST := $(ICONROOT)/scalable/apps/veter.svg

# Skeleton config. The live copy lands in the user's config dir (the same
# path veter reads at startup) only when absent, so edits survive
# re-installs; a pristine reference copy is always refreshed under
# $(PREFIX)/share/veter.
CONFIGDIR ?= $(if $(XDG_CONFIG_HOME),$(XDG_CONFIG_HOME),$(HOME)/.config)/veter
EXAMPLE_CONFIG_SRC := $(CURDIR)/assets/config.toml
EXAMPLE_CONFIG_DST := $(PREFIX)/share/veter/config.example.toml
USER_CONFIG_DST := $(CONFIGDIR)/config.toml

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
        install-config dist-clean install-dist-maybe \
        dist-aarch64-build dist-aarch64-tarxz dist-aarch64-deb \
        dist-aarch64-manifest install-remote-aarch64 \
        dist-amd64-build dist-amd64-tarxz dist-amd64-deb \
        dist-amd64-manifest install-remote-amd64

all: install

help:
	@echo "Targets:"
	@echo "  build               cargo build --release for $(PACKAGES)"
	@echo "  install             build and copy binaries into \$$BINDIR + desktop entry"
	@echo "                      + skeleton config into \$$CONFIGDIR (if absent)"
	@echo "  uninstall           remove installed binaries and desktop entry"
	@echo "  clean               cargo clean"
	@echo
	@echo "  dist-<arch>-build       cross-compile vmux/vcat/vplay/vsend/vrecv/vsd"
	@echo "                          for <arch>-unknown-linux-musl (static, rust-lld)"
	@echo "  dist-<arch>-tarxz       bundle the above into a .tar.xz under dist/"
	@echo "  dist-<arch>-manifest    write a sha256-stamped manifest beside the tarball"
	@echo "  dist-<arch>-deb         bundle the above into veter-tools_<v>_<debarch>.deb"
	@echo "  install-remote-<arch>   cross-compile and scp+install the <arch>-musl"
	@echo "                          binaries into REMOTE_BINDIR on REMOTE (over ssh)"
	@echo "  (<arch> ∈ aarch64, amd64; debarch ∈ arm64, amd64)"
	@echo
	@echo "  install-dist-maybe  stage tarball + manifest under \$$PREFIX/share/veter/dist/"
	@echo "                      for whichever rust-std musl targets are installed"
	@echo "                      (vssh reads them from there to deploy remotely)"
	@echo "  dist-clean          rm -rf dist/"
	@echo
	@echo "Variables (override on the command line):"
	@echo "  PREFIX=$(PREFIX)"
	@echo "  BINDIR=$(BINDIR)"
	@echo "  APPDIR=$(APPDIR)"
	@echo "  DIST_VERSION=$(DIST_VERSION)"
	@echo "  REMOTE=$(REMOTE)  (e.g. ha@home-assistant.local — required for install-remote-<arch>)"
	@echo "  REMOTE_BINDIR=$(REMOTE_BINDIR)"

build:
	$(CARGO) build --release $(addprefix --package=,$(PACKAGES))

# Each binary depends on the build step. Listing them as separate
# prerequisites of `install` lets make report a clear error if any one
# is missing after the build.
$(BINS): build

install: $(BINS) install-desktop install-icon install-config install-dist-maybe
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

# Install the commented skeleton config. A pristine reference copy is
# always (re)written to $(EXAMPLE_CONFIG_DST); the live copy is dropped
# into $(USER_CONFIG_DST) only if none exists yet, so re-running install
# never overwrites your edits.
install-config:
	@$(INSTALL) -d "$(dir $(EXAMPLE_CONFIG_DST))"
	@$(INSTALL) -m 0644 "$(EXAMPLE_CONFIG_SRC)" "$(EXAMPLE_CONFIG_DST)"
	@echo "    config.example.toml -> $(EXAMPLE_CONFIG_DST)"
	@if [ -f "$(USER_CONFIG_DST)" ]; then \
	    echo "    config.toml already at $(USER_CONFIG_DST) (left unchanged)"; \
	else \
	    $(INSTALL) -d "$(CONFIGDIR)"; \
	    $(INSTALL) -m 0644 "$(EXAMPLE_CONFIG_SRC)" "$(USER_CONFIG_DST)"; \
	    echo "    config.toml -> $(USER_CONFIG_DST)"; \
	fi

uninstall:
	@for pkg in $(PACKAGES); do \
	    rm -f "$(BINDIR)/$$pkg" && echo "    removed $(BINDIR)/$$pkg"; \
	done
	@if [ -d "$(PREFIX)/share/veter/dist" ]; then \
	    rm -rf "$(PREFIX)/share/veter/dist" && \
	    echo "    removed $(PREFIX)/share/veter/dist"; \
	fi
	@if [ -f "$(EXAMPLE_CONFIG_DST)" ]; then \
	    rm -f "$(EXAMPLE_CONFIG_DST)" && echo "    removed $(EXAMPLE_CONFIG_DST)"; \
	fi
	@if [ -f "$(USER_CONFIG_DST)" ]; then \
	    echo "    kept user config $(USER_CONFIG_DST) (remove by hand if unwanted)"; \
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

# ---- musl-static distribution of client-side tools ------------------
#
# Cross-builds vmux, vcat, vplay, vsend, vrecv, vsd for the musl-static
# targets enumerated in DIST_ARCHES, using rust-lld (which ships with
# rustup-installed rustc, so no host toolchain prereq beyond
# `rustup target add`). The resulting binaries are fully static — no
# dynamic loader, no libc dependency on the target system — and ride
# into either a .tar.xz or a .deb. Per-arch targets are emitted by the
# DIST_ARCH_RULES macro below.

DIST_TOOLS := vmux vcat vplay vsend vrecv vsd
DIST_VERSION ?= 0.1.7

# `install-remote-<arch>` knobs. `REMOTE` is required — it's whatever
# ssh(1) would accept (`user@host`, `host`, or a `Host` alias from
# `~/.ssh/config`). `REMOTE_BINDIR` is where the binaries land on the
# remote; ~/.local/bin keeps things user-scoped, no sudo needed. Both
# variables can be set on the make CLI or in the environment.
REMOTE ?=
REMOTE_BINDIR ?= ~/.local/bin
DIST_DIR := $(CURDIR)/dist
DIST_MAINTAINER := Alexey Denisov <rtgbnm@gmail.com>

# Triples we know how to cross-build. install-dist-maybe iterates this
# list and stages whichever rust-std targets the user has installed.
DIST_ARCHES := aarch64-unknown-linux-musl x86_64-unknown-linux-musl

# DIST_ARCH_RULES(triple, deb_arch, short, uname_arch) — emit the full
# build/tarxz/manifest/deb/install-remote/install-dist-share quintet
# for one musl target.
#   $(1) rust target triple             (e.g. aarch64-unknown-linux-musl)
#   $(2) debian Architecture: token     (e.g. arm64)
#   $(3) user-facing short suffix       (e.g. aarch64, amd64)
#   $(4) `uname -m` value, for README   (e.g. aarch64, x86_64)
define DIST_ARCH_RULES
.PHONY: dist-$(3)-build dist-$(3)-tarxz dist-$(3)-manifest dist-$(3)-deb \
        install-dist-share-$(1) install-remote-$(3)

DIST_BINDIR_$(1) := $$(TARGET_DIR)/$(1)/release
DIST_TARXZ_STAGING_$(1) := $$(DIST_DIR)/staging-tarxz-$(1)
DIST_TARXZ_FILE_$(1) := $$(DIST_DIR)/veter-tools-$$(DIST_VERSION)-$(1).tar.xz
DIST_MANIFEST_FILE_$(1) := $$(DIST_DIR)/manifest-$(1).json
DIST_DEB_STAGING_$(1) := $$(DIST_DIR)/veter-tools_$$(DIST_VERSION)_$(2)
DIST_DEB_FILE_$(1) := $$(DIST_DIR)/veter-tools_$$(DIST_VERSION)_$(2).deb
SHARE_DIST_DIR_$(1) := $$(PREFIX)/share/veter/dist/$(1)

dist-$(3)-build:
	@# Linker for this target is configured in `.cargo/config.toml`:
	@# `linker = "rust-lld"`. rust-lld ships inside the rustup
	@# toolchain so no host cross-gcc is required.
	@rustup target list --installed 2>/dev/null | grep -qx '$(1)' || { \
	    echo "Installing rust-std for $(1)..."; \
	    rustup target add $(1); \
	}
	$$(CARGO) build --release --target $(1) \
	    $$(addprefix --package=,$$(DIST_TOOLS))
	@for t in $$(DIST_TOOLS); do \
	    src="$$(DIST_BINDIR_$(1))/$$$$t"; \
	    if [ ! -x "$$$$src" ]; then \
	        echo "error: $$$$t binary not found at $$$$src" >&2; \
	        exit 1; \
	    fi; \
	done

dist-$(3)-tarxz: dist-$(3)-build
	@$$(INSTALL) -d $$(DIST_DIR)
	@rm -rf $$(DIST_TARXZ_STAGING_$(1))
	@$$(INSTALL) -d $$(DIST_TARXZ_STAGING_$(1))/veter-tools-$$(DIST_VERSION)
	@for t in $$(DIST_TOOLS); do \
	    $$(INSTALL) -m 0755 $$(DIST_BINDIR_$(1))/$$$$t \
	        $$(DIST_TARXZ_STAGING_$(1))/veter-tools-$$(DIST_VERSION)/$$$$t; \
	done
	@printf '%s\n' \
	    'veter-tools $$(DIST_VERSION) — $(1)' \
	    '' \
	    'Remote-side tools for the Veter terminal emulator. Statically' \
	    'linked against musl libc; drop the binaries anywhere on $$$$PATH on' \
	    'a $(4) Linux host and they will work without any runtime' \
	    'dependencies.' \
	    '' \
	    'Tools:' \
	    '  vmux    terminal multiplexer (PRT + VGE)' \
	    '  vcat    display images inline (VGE)' \
	    '  vplay   interactive image/video viewer (VGE; needs ffmpeg)' \
	    '  vsend   upload local files (VFT)' \
	    '  vrecv   download remote files (VFT)' \
	    '  vsd  persistent session daemon (doc/session-manager.md)' \
	    > $$(DIST_TARXZ_STAGING_$(1))/veter-tools-$$(DIST_VERSION)/README
	@tar -cJf $$(DIST_TARXZ_FILE_$(1)) \
	    -C $$(DIST_TARXZ_STAGING_$(1)) veter-tools-$$(DIST_VERSION)
	@rm -rf $$(DIST_TARXZ_STAGING_$(1))
	@echo "    veter-tools tarball -> $$(DIST_TARXZ_FILE_$(1))"
	@ls -l $$(DIST_TARXZ_FILE_$(1))

# Manifest is what vssh reads to decide whether to upload to a remote.
# Carrying the sha256 of the tarball (not just the version string)
# lets us treat any content change as a reason to push, including
# rebuilds where the version number didn't move.
dist-$(3)-manifest: dist-$(3)-tarxz
	@sha=$$$$(sha256sum $$(DIST_TARXZ_FILE_$(1)) | cut -d' ' -f1); \
	tools=$$$$(printf '"%s",' $$(DIST_TOOLS) | sed 's/,$$$$//'); \
	printf '{"version":"%s","arch":"%s","sha256":"%s","tools":[%s]}\n' \
	    "$$(DIST_VERSION)" "$(1)" "$$$$sha" "$$$$tools" \
	    > $$(DIST_MANIFEST_FILE_$(1))
	@echo "    manifest -> $$(DIST_MANIFEST_FILE_$(1))"

install-dist-share-$(1): dist-$(3)-manifest
	@$$(INSTALL) -d $$(SHARE_DIST_DIR_$(1))
	@$$(INSTALL) -m 0644 $$(DIST_TARXZ_FILE_$(1)) $$(SHARE_DIST_DIR_$(1))/veter-tools.tar.xz
	@$$(INSTALL) -m 0644 $$(DIST_MANIFEST_FILE_$(1)) $$(SHARE_DIST_DIR_$(1))/manifest.json
	@echo "    dist tarball -> $$(SHARE_DIST_DIR_$(1))/veter-tools.tar.xz"
	@echo "    dist manifest -> $$(SHARE_DIST_DIR_$(1))/manifest.json"

dist-$(3)-deb: dist-$(3)-build
	@command -v dpkg-deb >/dev/null 2>&1 || { \
	    echo "error: dpkg-deb required; install the 'dpkg' package" >&2; \
	    exit 1; \
	}
	@$$(INSTALL) -d $$(DIST_DIR)
	@rm -rf $$(DIST_DEB_STAGING_$(1))
	@$$(INSTALL) -d $$(DIST_DEB_STAGING_$(1))/DEBIAN $$(DIST_DEB_STAGING_$(1))/usr/bin
	@for t in $$(DIST_TOOLS); do \
	    $$(INSTALL) -m 0755 $$(DIST_BINDIR_$(1))/$$$$t $$(DIST_DEB_STAGING_$(1))/usr/bin/$$$$t; \
	done
	@printf '%s\n' \
	    'Package: veter-tools' \
	    'Version: $$(DIST_VERSION)' \
	    'Section: utils' \
	    'Priority: optional' \
	    'Architecture: $(2)' \
	    'Maintainer: $$(DIST_MAINTAINER)' \
	    'Description: Remote-side tools for the Veter terminal emulator' \
	    ' Statically-linked $(4) binaries for vmux, vcat, vplay, vsend,' \
	    ' vrecv, and vsd. The first five talk PRT/VGE/VFT to a' \
	    ' Veter-aware terminal (or to vmux running inside one); vsd is' \
	    ' a persistent session daemon that owns inner PTYs across renderer' \
	    ' attach/detach cycles (see doc/session-manager.md). The binaries' \
	    ' have no runtime dependencies on the target system, except vplay' \
	    ' which invokes ffmpeg/ffprobe for video playback.' \
	    > $$(DIST_DEB_STAGING_$(1))/DEBIAN/control
	@dpkg-deb --root-owner-group --build $$(DIST_DEB_STAGING_$(1)) $$(DIST_DEB_FILE_$(1)) >/dev/null
	@rm -rf $$(DIST_DEB_STAGING_$(1))
	@echo "    veter-tools deb -> $$(DIST_DEB_FILE_$(1))"
	@ls -l $$(DIST_DEB_FILE_$(1))

# One-shot deploy to a remote $(4) host. Streams the freshly-built
# release binaries through a single ssh connection (no intermediate
# tarball lands on disk), then mkdirs REMOTE_BINDIR and untars there.
# Permissions are preserved by tar(1); the binaries land 0755.
#
# Usage:
#   make install-remote-$(3) REMOTE=user@host
#   make install-remote-$(3) REMOTE=user@host REMOTE_BINDIR=/opt/veter/bin
install-remote-$(3): dist-$(3)-build
	@if [ -z "$$(REMOTE)" ]; then \
	    echo "error: REMOTE not set" >&2; \
	    echo "  example: make install-remote-$(3) REMOTE=user@host" >&2; \
	    exit 1; \
	fi
	@echo "    veter-tools -> $$(REMOTE):$$(REMOTE_BINDIR)/"
	@tar -cf - -C $$(DIST_BINDIR_$(1)) $$(DIST_TOOLS) \
	    | ssh $$(REMOTE) "mkdir -p $$(REMOTE_BINDIR) && tar -xpf - -C $$(REMOTE_BINDIR)"
	@for t in $$(DIST_TOOLS); do echo "    $$$$t -> $$(REMOTE):$$(REMOTE_BINDIR)/$$$$t"; done

endef

$(eval $(call DIST_ARCH_RULES,aarch64-unknown-linux-musl,arm64,aarch64,aarch64))
$(eval $(call DIST_ARCH_RULES,x86_64-unknown-linux-musl,amd64,amd64,x86_64))

# Run as a dep of `install`. For each musl target in DIST_ARCHES whose
# rust-std is installed, stage the matching dist bundle under
# $PREFIX/share/veter/dist/<triple>/. Targets with no rust-std are
# skipped with a note; if none are installed, `vssh` falls back to a
# thin ssh wrapper without auto-deploy.
install-dist-maybe:
	@any=0; \
	for triple in $(DIST_ARCHES); do \
	    if rustup target list --installed 2>/dev/null | grep -qx "$$triple"; then \
	        $(MAKE) --no-print-directory install-dist-share-$$triple; \
	        any=1; \
	    else \
	        echo "note: $$triple rust-std not installed; skipping its bundle."; \
	    fi; \
	done; \
	if [ "$$any" = "0" ]; then \
	    echo "note: no musl rust-std installed; vssh will run without bundled remote tools."; \
	    echo "      run 'rustup target add <triple>' for any of: $(DIST_ARCHES)"; \
	fi

dist-clean:
	@rm -rf $(DIST_DIR)
	@echo "    removed $(DIST_DIR)"
