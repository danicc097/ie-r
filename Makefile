.PHONY: all setup-arch build build-release test clean install install-github-release release size

BINDIR := target/release
BINNAME := ie-r
INSTALLDIR := $(HOME)/.local/bin
DESKTOPDIR := $(HOME)/.local/share/applications
ICONDIR := $(HOME)/.local/share/icons/hicolor
GH_REPO ?= danicc097/ie-r

all: build

setup-arch:
	@echo "Installing Arch Linux build dependencies..."
	sudo pacman -S --needed --noconfirm base-devel clang dbus fontconfig libx11 \
		libxcursor libxi libxkbcommon libxrandr pkgconf rust wayland

build:
	cargo build

build-release:
	cargo build --release --locked

test:
	cargo test --workspace -- --test-threads=1

install: build-release
	install -Dm755 "$(BINDIR)/$(BINNAME)" "$(INSTALLDIR)/$(BINNAME)"
	install -Dm644 assets/ie-r.desktop "$(DESKTOPDIR)/ie-r.desktop"
	sed -i 's|^Exec=.*|Exec=$(INSTALLDIR)/$(BINNAME)|' "$(DESKTOPDIR)/ie-r.desktop"
	install -Dm644 assets/ie-r.svg "$(ICONDIR)/scalable/apps/ie-r.svg"
	install -Dm644 assets/ie-r-64x64.png "$(ICONDIR)/64x64/apps/ie-r.png"

install-github-release:
	@command -v gh >/dev/null 2>&1 || { echo "Error: GitHub CLI (gh) is required."; exit 1; }
	@tmp="$$(mktemp -d)"; \
	trap 'rm -rf "$$tmp"' EXIT; \
	gh release download --repo "$(GH_REPO)" --pattern "ie-r-archlinux-x86_64" --dir "$$tmp"; \
	install -Dm755 "$$tmp/ie-r-archlinux-x86_64" "$(INSTALLDIR)/$(BINNAME)"

release: install-github-release

size: build-release
	ls -lh "$(BINDIR)/$(BINNAME)"

clean:
	cargo clean