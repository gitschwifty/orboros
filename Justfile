set shell := ["bash", "-cu"]

# Show available recipes.
default:
    @just --list

# Create benchmark corpus directories. Override root with:
# `just bench-init ../bench`.
bench-init root="bench":
    mkdir -p "{{root}}/cases/t1" "{{root}}/cases/t2" "{{root}}/cases/t3" "{{root}}/fixtures" "{{root}}/prompts" "{{root}}/results"

# List local benchmark cases.
bench-list root="bench":
    cargo run -- bench --bench-root "{{root}}" list

# Run all benchmark cases, or pass a tier: `just bench-run t1`.
bench-run tier="" root="bench":
    @if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}"; \
    else \
        cargo run -- bench --bench-root "{{root}}" run; \
    fi

# Run benchmark cases with an explicit model/variant tag.
bench-run-model model variant tier="" root="bench":
    @if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}" --model "{{model}}" --variant "{{variant}}"; \
    else \
        cargo run -- bench --bench-root "{{root}}" run --model "{{model}}" --variant "{{variant}}"; \
    fi

# Run one benchmark case by id.
bench-case id root="bench":
    cargo run -- bench --bench-root "{{root}}" run --case "{{id}}"

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
