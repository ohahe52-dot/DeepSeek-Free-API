# Justfile for ai-free-api

set positional-arguments

# Run all checks: type check, lint, format, audit, unused deps
# Điều kiện trước: cargo install cargo-audit && cargo install cargo-machete && cargo install cargo-outdated
check:
  cargo fmt --check      
  cargo check            
  cargo clippy -- -D warnings  
  cargo audit --deny warnings
  cargo outdated --exit-code 1 --root-deps-only
  cargo machete          

# Build + lint frontend (bun install --frozen-lockfile, bun run typecheck + build + lint)
check-web:
  cd web && bun install --frozen-lockfile && bun run typecheck && bun run build && bun run lint


# Run unified protocol debug CLI (replaces ds-core-cli / openai-adapter-cli)
# Mặc định dùng py-e2e-tests/config.toml, có thể override bằng -c <path>
adapter-cli *ARGS:
  cargo run --example adapter_cli -- -c py-e2e-tests/config.toml "$@"

# Run openai_adapter/request submodule tests
test-adapter-request *ARGS:
  cargo test openai_adapter::request -- "$@"

# Run openai_adapter/response submodule tests
test-adapter-response *ARGS:
  cargo test openai_adapter::response -- "$@"

# Run HTTP server (tự build frontend mới nhất -> khởi động backend)
serve *ARGS:
  (cd web && bun run build) && cargo run -- "$@"

# Basic: test chức năng cơ bản (hai endpoint)
e2e-basic *ARGS:
  cd py-e2e-tests && uv run python runner.py scenarios/basic "$@"

# Repair: test chuyên biệt sửa lỗi tool call hỏng
e2e-repair *ARGS:
  cd py-e2e-tests && uv run python runner.py scenarios/repair "$@"

# Stress: test tải nhiều vòng lặp đồng thời (mọi scenario basic + repair)
e2e-stress *ARGS:
  cd py-e2e-tests && uv run python stress_runner.py "$@"

# Oversized: test fallback ngữ cảnh dài (expert chia chunk + default/vision upload file)
e2e-oversized *ARGS:
  cd py-e2e-tests && uv run python test_oversized.py "$@"

# Start server with e2e test config
e2e-serve:
  (cd web && bun run build) && cargo run -- -c py-e2e-tests/config.toml
