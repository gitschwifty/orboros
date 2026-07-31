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

# Run all benchmark cases, optionally selecting a prompt set:
# `just bench-run t2 ../orboros-bench composable-v1`.
bench-run tier="" root="bench" prompt_set="":
    @prompt_arg=""; \
    if [ -n "{{prompt_set}}" ]; then prompt_arg='--prompt-set "{{prompt_set}}"'; fi; \
    if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}" $prompt_arg; \
    else \
        cargo run -- bench --bench-root "{{root}}" run $prompt_arg; \
    fi

# Run a tier with a prompt set while retaining the default bench root:
# `just bench-run-prompt composable-v1 t2`.
bench-run-prompt prompt_set tier="" root="bench":
    @if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}" --prompt-set "{{prompt_set}}"; \
    else \
        cargo run -- bench --bench-root "{{root}}" run --prompt-set "{{prompt_set}}"; \
    fi

# Build the release binary.
build-release:
    cargo build --release

# Build release, then run benchmark cases with a default model.
bench-run-release model="openrouter/free" tier="" variant="" root="bench" prompt_set="": build-release
    @variant_arg=""; \
    prompt_arg=""; \
    if [ -n "{{variant}}" ]; then variant_arg='--variant "{{variant}}"'; fi; \
    if [ -n "{{prompt_set}}" ]; then prompt_arg='--prompt-set "{{prompt_set}}"'; fi; \
    if [ -n "{{tier}}" ]; then \
        ./target/release/orboros bench --bench-root "{{root}}" run --tier "{{tier}}" --model "{{model}}" $variant_arg $prompt_arg; \
    else \
        ./target/release/orboros bench --bench-root "{{root}}" run --model "{{model}}" $variant_arg $prompt_arg; \
    fi

# Run benchmark cases with an explicit model. Empty tier runs all cases.
bench-run-model model tier="" variant="" root="bench" prompt_set="":
    @variant_arg=""; \
    prompt_arg=""; \
    if [ -n "{{variant}}" ]; then variant_arg='--variant "{{variant}}"'; fi; \
    if [ -n "{{prompt_set}}" ]; then prompt_arg='--prompt-set "{{prompt_set}}"'; fi; \
    if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}" --model "{{model}}" $variant_arg $prompt_arg; \
    else \
        cargo run -- bench --bench-root "{{root}}" run --model "{{model}}" $variant_arg $prompt_arg; \
    fi

# Run an explicit model with a prompt set:
# `just bench-run-model-prompt openrouter/free composable-v1 t2`.
bench-run-model-prompt model prompt_set tier="" variant="" root="bench":
    @variant_arg=""; \
    if [ -n "{{variant}}" ]; then variant_arg='--variant "{{variant}}"'; fi; \
    if [ -n "{{tier}}" ]; then \
        cargo run -- bench --bench-root "{{root}}" run --tier "{{tier}}" --model "{{model}}" --prompt-set "{{prompt_set}}" $variant_arg; \
    else \
        cargo run -- bench --bench-root "{{root}}" run --model "{{model}}" --prompt-set "{{prompt_set}}" $variant_arg; \
    fi

# Build release, then run benchmark cases with an explicit model.
bench-run-model-release model tier="" variant="" root="bench" prompt_set="": build-release
    @variant_arg=""; \
    prompt_arg=""; \
    if [ -n "{{variant}}" ]; then variant_arg='--variant "{{variant}}"'; fi; \
    if [ -n "{{prompt_set}}" ]; then prompt_arg='--prompt-set "{{prompt_set}}"'; fi; \
    if [ -n "{{tier}}" ]; then \
        ./target/release/orboros bench --bench-root "{{root}}" run --tier "{{tier}}" --model "{{model}}" $variant_arg $prompt_arg; \
    else \
        ./target/release/orboros bench --bench-root "{{root}}" run --model "{{model}}" $variant_arg $prompt_arg; \
    fi

# Run one benchmark case by id, optionally selecting a prompt set.
bench-case id root="bench" prompt_set="":
    @prompt_arg=""; \
    if [ -n "{{prompt_set}}" ]; then prompt_arg='--prompt-set "{{prompt_set}}"'; fi; \
    cargo run -- bench --bench-root "{{root}}" run --case "{{id}}" $prompt_arg

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

# Run env-gated live Heddle IPC tests.
# Examples:
# `just test-heddle ../heddle-headless`
# `just test-heddle ../heddle-headless openrouter/free`
# `just test-heddle ../heddle-headless anthropic/claude-haiku-4.5 1`
test-heddle binary="" model="" expect_cost="":
    @if [ -n "{{binary}}" ]; then export HEDDLE_BINARY="{{binary}}"; fi; \
    if [ -n "{{model}}" ]; then export HEDDLE_TEST_MODEL="{{model}}"; fi; \
    if [ -n "{{expect_cost}}" ]; then export HEDDLE_EXPECT_COST="{{expect_cost}}"; fi; \
    if [ -z "$${HEDDLE_BINARY:-}" ]; then \
        echo "HEDDLE_BINARY unset; live Heddle tests will skip"; \
    fi; \
    cargo test --test worker_lifecycle -- --nocapture

# Full local verification gate.
ci: fmt-check clippy test
