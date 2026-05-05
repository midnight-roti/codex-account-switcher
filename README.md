# Codex Account Switcher

A Rust terminal UI for managing multiple Codex accounts, checking quota quickly, and switching the active account used by Codex.

## Main Features

- View all signed-in accounts in one interface
- See both 5-hour and weekly quota for each account
- Sort accounts by remaining quota and hide exhausted accounts by default
- Add and delete accounts from the app
- Refresh the selected account or all accounts from the keyboard
- Apply the selected account to Codex
- Search accounts and filter by plan type

## Install

Local clone:

```powershell
cargo install --path .
```

Direct from GitHub:

```powershell
cargo install --git https://github.com/midnight-roti/codex-account-switcher --bin cas
```

Do not run `cargo install cas` unless this crate is published under that name on crates.io. That command installs a different crate from the registry.

## Run

```powershell
cargo run
```

Installed binary:

```powershell
cas
```

## Build

```powershell
cargo build --release
```

## License

MIT. See [LICENSE](LICENSE).


