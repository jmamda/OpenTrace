# Contributing to OpenTrace

Thank you for your interest in contributing to OpenTrace! This document outlines guidelines for contributing to this project.

## Getting Started

1. Fork the repository and clone it locally.
2. Install dependencies: `npm install -g @opentraceai/trace` (for the npm package) or build from source with `cargo build`.
3. Create a new branch for your feature or bug fix: `git checkout -b feature/your-feature-name`.

## How to Contribute

### Reporting Bugs

- Search existing [issues](https://github.com/jmamda/OpenTrace/issues) before opening a new one.
- Include a clear title, description, steps to reproduce, expected vs. actual behavior, and your environment details.

### Suggesting Features

- Open an issue with the label `enhancement` and describe the use case clearly.

### Submitting Pull Requests

1. Ensure your code builds without errors (`cargo build`).
2. Run existing tests: `cargo test`.
3. Keep commits focused and write clear commit messages.
4. Reference any related issues in your PR description.
5. PRs should target the `main` branch.

## Code Style

- Follow standard Rust formatting: run `cargo fmt` before committing.
- Lint with `cargo clippy` and address any warnings.
- For the npm/dashboard portion, follow existing code conventions.

## Development Setup

```bash
# Clone the repo
git clone https://github.com/jmamda/OpenTrace.git
cd OpenTrace

# Build the Rust binary
cargo build --release

# Run tests
cargo test
```

## Code of Conduct

This project follows a [Code of Conduct](CODE_OF_CONDUCT.md). By participating, you agree to uphold it.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
