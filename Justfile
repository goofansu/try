# Development tasks. `just` on its own lists them.

[private]
default:
    @just --list

# Everything CI runs, in the order it runs it.
ci: lint test

# Lint, with warnings as errors, the way CI does.
lint:
    cargo clippy --all-targets --locked -- -D warnings

# Run the tests.
test:
    cargo test --locked
