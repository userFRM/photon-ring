# Release Process

## Pre-release checklist
1. All agents complete their work
2. `cargo fmt --all -- --check`
3. `cargo test` (all pass)
4. `cargo clippy --all-targets -- -D warnings`
5. `cargo check --target wasm32-unknown-unknown`
6. **Codex CLI review loop:**
   - Submit full crate to `codex exec --full-auto` for review
   - Fix every finding
   - Re-submit to Codex
   - Repeat until Codex says "ready to ship"
7. Bump version in Cargo.toml
8. Update CHANGELOG.md
9. `git commit`, `git push`
10. `cargo publish`
11. `gh release create`
