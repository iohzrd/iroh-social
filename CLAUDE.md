# CLAUDE.md

## Rules

- **DON'T BOTHER WITH BACKWARD COMPATIBILITY.**
- Always thoroghly study all existing code relavant to your current task before offering changes.
- **NEVER USE EMOJIS** in code, documentation, commits, anywhere
- Always use latest dependency versions.
- Always run code formatters after making changes (`cargo fmt` for backend, `npm run check` and `npx prettier --plugin prettier-plugin-svelte --write "src/**/*.{ts,svelte}" 2>&1` for frontend)
- Always run `cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings` before committing and fix any warnings.
- Always omit Claude signature when writing commit messages.
- Always follow Svelte 5 idioms and best practices. See [docs/svelte5-llms-small.txt](docs/svelte5-llms-small.txt) for reference.
- Always follow Rust idioms and best practices.

## Commands

- When the user says "review and commit", this means review ALL the uncommited changes with git diff, then commit.
- When the user says "review and report", this means re-inspect ALL the code relevant to the current task and revise your list on current options.
