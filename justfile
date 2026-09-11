set positional-arguments

# Build and run the release version. Forward arguments to tpane.
run *args:
    ./scripts/cargo.sh run --release --locked -- "$@"
