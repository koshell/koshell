# koshell — build and install both runtimes (Rust terminal + compiled Bun AI daemon).
#
# Targets:
#   all        build both binaries (default)
#   koshell    build only the Rust terminal   -> target/release/koshell
#   daemon     build only the AI daemon       -> packages/ai-daemon/dist/koshell-ai-daemon
#   debug      debug build of the Rust terminal -> target/debug/koshell
#   install    install both binaries and man pages under $(DESTDIR)$(PREFIX)
#   uninstall  remove exactly the files install created
#   check      run the repo's full validation (same commands as CI)
#   clean      remove release build artifacts
#
# Variables (override on the command line):
#   PREFIX    install root, default /usr/local   e.g. make install PREFIX=$$HOME/.local
#   DESTDIR   staging root for packaging         e.g. make install DESTDIR=/tmp/stage
#
# The terminal finds the daemon by looking for a koshell-ai-daemon executable next
# to the koshell binary (then on PATH), so installing both into $(BINDIR) makes
# daemon auto-spawn work with zero configuration.

PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
MANDIR  ?= $(PREFIX)/share/man
MAN1DIR  = $(MANDIR)/man1
MAN5DIR  = $(MANDIR)/man5

CARGO ?= cargo
BUN   ?= bun

KOSHELL_BIN = target/release/koshell
DAEMON_BIN  = packages/ai-daemon/dist/koshell-ai-daemon

.PHONY: all koshell daemon debug install uninstall check clean

all: koshell daemon

koshell:
	$(CARGO) build --release

daemon:
	$(BUN) install --frozen-lockfile
	cd packages/ai-daemon && $(BUN) run build:binary

# The daemon has no debug variant: in development it runs straight from source
# (export KOSHELL_DAEMON_CMD="bun $$PWD/packages/ai-daemon/src/index.ts").
debug:
	$(CARGO) build

# install deliberately does not depend on the build targets: the natural flow is
# `make && sudo make install`, and rebuilding under sudo would leave root-owned
# files in target/. The guards turn a missing artifact into a clear error instead.
install:
	@test -x $(KOSHELL_BIN) || { echo "error: $(KOSHELL_BIN) missing; run 'make' first" >&2; exit 1; }
	@test -x $(DAEMON_BIN) || { echo "error: $(DAEMON_BIN) missing; run 'make' first" >&2; exit 1; }
	install -d $(DESTDIR)$(BINDIR) $(DESTDIR)$(MAN1DIR) $(DESTDIR)$(MAN5DIR)
	install -m 0755 $(KOSHELL_BIN) $(DESTDIR)$(BINDIR)/koshell
	install -m 0755 $(DAEMON_BIN) $(DESTDIR)$(BINDIR)/koshell-ai-daemon
	install -m 0644 man/koshell.1 $(DESTDIR)$(MAN1DIR)/koshell.1
	install -m 0644 man/koshell.toml.5 $(DESTDIR)$(MAN5DIR)/koshell.toml.5

# Removes exactly what install created; never touches user config or state.
uninstall:
	rm -f $(DESTDIR)$(BINDIR)/koshell
	rm -f $(DESTDIR)$(BINDIR)/koshell-ai-daemon
	rm -f $(DESTDIR)$(MAN1DIR)/koshell.1
	rm -f $(DESTDIR)$(MAN5DIR)/koshell.toml.5

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test
	$(BUN) install --frozen-lockfile
	$(BUN) run check

# cargo clean --release keeps target/debug (daily cargo build/test caches) intact.
clean:
	$(CARGO) clean --release
	rm -rf packages/ai-daemon/dist
