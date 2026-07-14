set shell := ["bash", "-cu"]

# Show available recipes.
default:
    @just --list

# Create the local, gitignored benchmark corpus directories.
bench-init:
    mkdir -p bench/cases/t1 bench/cases/t2 bench/cases/t3 bench/fixtures bench/prompts bench/results

# List local benchmark cases from bench/cases.
bench-list:
    cargo run -- bench list

# Run all benchmark cases, or pass a tier: `just bench-run t1`.
bench-run tier="":
    @if [ -n "{{tier}}" ]; then \
        cargo run -- bench run --tier "{{tier}}"; \
    else \
        cargo run -- bench run; \
    fi

# Run one benchmark case by id.
bench-case id:
    cargo run -- bench run --case "{{id}}"

# Show a saved benchmark run.
bench-show run_id:
    cargo run -- bench show "{{run_id}}"

# Compare two saved benchmark runs.
bench-compare run_a run_b:
    cargo run -- bench compare "{{run_a}}" "{{run_b}}"

# List saved benchmark runs.
bench-runs:
    cargo run -- bench list-runs

# Show confidence calibration for a saved benchmark run.
bench-calibration run_id buckets="10":
    cargo run -- bench calibration "{{run_id}}" --buckets "{{buckets}}"

# Fast compile check.
check:
    cargo check

# Format code.
fmt:
    cargo fmt

# Check formatting without changing files.
fmt-check:
    cargo fmt --check

# Run clippy with the repo's warning policy.
clippy:
    cargo clippy --all-targets -- -D warnings

# Run tests.
test:
    cargo test

# Full local verification gate.
ci: fmt-check clippy test
