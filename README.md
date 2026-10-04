# Mobius

Mobius is a self-hosted server for one Owner. It runs agents on your own Harness subscriptions (Claude Code, Antigravity, and Devin), and it keeps the work on GitHub issues and pull requests.

To install Mobius, read [docs/install.md](docs/install.md).

To build Mobius, install `sccache`. It caches the compiled dependencies, so all worktrees share them. It does not cache the incremental build of the workspace crates. A build fails when `sccache` is not on `PATH`. Use one of these commands:

```sh
brew install sccache
cargo install sccache --locked
cargo binstall sccache
```
