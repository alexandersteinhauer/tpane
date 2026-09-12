set positional-arguments

# Build and run the release version. Forward arguments to tpane.
run *args:
    ./scripts/cargo.sh run --release --locked -- "$@"

# Compare native libghostty / tpane / tmux / zellij (all costs relative to native).
bench *args:
    python3 scripts/bench.py "$@"

# Install the release binary into ~/.cargo/bin (already in PATH). Uses the
# toolchain pinned in rust-toolchain.toml explicitly, like every other build.
install:
    #!/bin/sh
    set -eu
    channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
    ./scripts/cargo.sh "+$channel" install --locked --path .
