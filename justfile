set positional-arguments

# Build and run the release version. Forward arguments to tpane.
run *args:
    ./scripts/cargo.sh run --release --locked -- "$@"

# Compare native libghostty / tpane / tmux / zellij (all costs relative to native).
bench *args:
    python3 scripts/bench.py "$@"
