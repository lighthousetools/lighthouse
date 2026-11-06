# Conventions

Lighthouse by Lighthouse Tools is a public open-source project.

- **Language:** code, identifiers, comments, commit messages and documentation are in English.
- **No internals:** never add company, customer, personal or infrastructure details. Examples stay generic, and
  user-specific values belong in deployment configuration.
- **Commits:** small, Conventional Commits style, directly on `main` during the build-up. No attribution trailers.
- **Rust:** stable, edition 2024, workspace lints, no `unsafe`. Use `thiserror` in libraries, `anyhow` only in binaries,
  and `tracing` for logs.
- **Tests:** every change comes with tests. Line coverage of at least 80% is enforced in CI.
- **Secrets:** keys never appear in the repo, logs, error messages or prompts.
- **Models:** model names and prices live only in configuration, never in code.
- **Names:** no program is called just `lighthouse`. The binaries are `lighthouse-server` and `lht`, the crates are
  `lighthouse-*`.
- **Docs:** new features, formats and conventions are documented in the same commit.
