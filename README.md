# Lighthouse by Lighthouse Tools

Lighthouse is an open-source personal assistant and agent system written in Rust. It runs self-hosted with your own
API keys, reads from your tools through MCP, and delivers a daily brief and a weekly report.

> **Status:** early development. Nothing here is ready for production yet.

## Development

Requirements: stable Rust and a local Postgres for the integration tests.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

## License

[MIT](LICENSE)
