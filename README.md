# sqsh

A TUI MySQL client for fast database exploration with fuzzy search, written in Rust.

## Features

- **Bastion Support**: Connect to MySQL through SSH bastion servers with per-connection or shared bastion configuration
- **TUI Interface**: Interactive terminal UI powered by ratatui
- **Fuzzy Finder**: Quick connection and table selection with skim (Rust-native fzf)
- **TOML Configuration**: Manage multiple connections in a single config file
- **Connection Pooling**: Configurable connection pool with global defaults and per-connection overrides
- **Shell Input**: Run shell commands from within sqsh (executes on bastion when connected via bastion)
- **Read-only Mode**: Prevent accidental writes with client-side SQL detection and server-side session enforcement
- **Secure**: Memory-zeroed password handling, TLS/SSL support, config file permission checks

## Installation

### Homebrew

```bash
brew tap hatohato25/sqsh
brew install sqsh
```

### From Source

```bash
cargo build --release
cp target/release/sqsh /usr/local/bin/
```

## Requirements

- Rust 1.75.0 or later (for building from source)
- MySQL 5.7+ / MariaDB 10.3+

## Configuration

Create a configuration file at `~/.config/sqsh/config.toml` and restrict its permissions:

```bash
chmod 600 ~/.config/sqsh/config.toml
```

sqsh warns if the file permissions are not `600`.

### Configuration Reference

#### `[settings]` — Application settings (optional)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `language` | string | `"en"` | Display language: `"en"` or `"ja"` |

#### `[[connections]]` — Connection entry (one or more required)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | — | Connection name (required) |
| `bastion` | see below | omitted | Bastion configuration |
| `readonly` | bool | `false` | Prevent write operations when `true` |

**`bastion` field behavior:**

| Value | Effect |
|-------|--------|
| Omitted or `false` | Direct connection (no bastion) |
| `true` | Use `[default_bastion]` settings |
| `[connections.bastion]` table | Use per-connection bastion settings |

#### `[connections.bastion]` — Per-connection bastion settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `host` | string | — | Bastion server hostname or IP (required) |
| `port` | integer | `22` | SSH port |
| `user` | string | — | SSH username (required) |
| `key_path` | string | — | Path to SSH private key; omit to use SSH agent |

#### `[connections.mysql]` — MySQL settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `host` | string | — | MySQL hostname or IP (required) |
| `port` | integer | `3306` | MySQL port |
| `database` | string | — | Database name (required) |
| `user` | string | — | MySQL username (required) |
| `password` | string | — | MySQL password (required) |
| `timeout` | integer | `30` | Connection timeout in seconds |
| `ssl_mode` | string | `"required"` | TLS/SSL mode: `"required"`, `"preferred"`, or `"disabled"` |

#### `[connections.mysql.pool]` — Connection pool settings (optional)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_connections` | integer | `10` | Maximum number of connections |
| `idle_timeout` | integer | `300` | Idle connection timeout in seconds |

#### `[default_bastion]` — Shared bastion settings (optional)

Applied to all connections with `bastion = true`. Fields are the same as `[connections.bastion]`.

#### `[default_mysql_pool]` — Shared pool settings (optional)

Applied to all connections that do not specify their own pool settings. Per-connection settings take precedence. Fields are the same as `[connections.mysql.pool]`.

### Configuration Examples

#### Example 1: Direct connection (local development)

```toml
[[connections]]
name = "local-dev"

[connections.mysql]
host = "localhost"
port = 3306
database = "your_database"
user = "root"
password = "your_password"
ssl_mode = "disabled"  # acceptable for local development
```

#### Example 2: Shared bastion (production environments)

Use `[default_bastion]` when multiple connections share the same bastion server.

```toml
[default_bastion]
host = "bastion.example.com"
port = 22
user = "your_ssh_user"
# Omit key_path to use your SSH agent (e.g., 1Password SSH agent)

[default_mysql_pool]
max_connections = 20
idle_timeout = 600

[[connections]]
name = "production"
bastion = true  # uses [default_bastion]

[connections.mysql]
host = "mysql.internal.example.com"
port = 3306
database = "production_db"
user = "app_user"
password = "secure_password"
timeout = 60
ssl_mode = "required"

[[connections]]
name = "staging"
bastion = true  # uses [default_bastion]
readonly = true

[connections.mysql]
host = "mysql-staging.internal.example.com"
port = 3306
database = "staging_db"
user = "app_user"
password = "staging_password"
ssl_mode = "preferred"
```

#### Example 3: Per-connection bastion

Use `[connections.bastion]` when each connection has a different bastion server.

```toml
[[connections]]
name = "region-a"

[connections.bastion]
host = "bastion-a.example.com"
port = 22
user = "your_ssh_user"
key_path = "~/.ssh/id_rsa"

[connections.mysql]
host = "mysql-a.internal.example.com"
port = 3306
database = "db_a"
user = "app_user"
password = "password_a"
ssl_mode = "required"

[[connections]]
name = "region-b"

[connections.bastion]
host = "bastion-b.example.com"
port = 2222
user = "your_ssh_user"
key_path = "~/.ssh/id_ed25519"

[connections.mysql]
host = "mysql-b.internal.example.com"
port = 3306
database = "db_b"
user = "app_user"
password = "password_b"
ssl_mode = "required"
```

See `config.example.toml` for a complete annotated example.

## Usage

```bash
sqsh                          # Start with default config (~/.config/sqsh/config.toml)
sqsh --config /path/to.toml   # Specify a config file
sqsh --verbose                # Enable debug logging
sqsh --readonly               # Start in read-only mode (overrides per-connection settings)
sqsh --lang ja                # Use Japanese display language
```

Config file search order when `--config` is not specified:
1. `~/.config/sqsh/config.toml`
2. `./config.toml`

## Key Bindings

### Connection Selection

Connection selection is handled by skim (Rust-native fzf). Standard fzf key bindings apply.

| Key | Action |
|-----|--------|
| Type to filter | Incremental search |
| `Up` / `Down` | Move cursor |
| `Enter` | Select connection |
| `ESC` / `Ctrl+C` | Cancel / quit |

### SQL Input

SQL input is handled directly by sqsh. Press `Tab` to switch focus between SQL Input and Shell Input.

#### Execution and Completion

| Key | Action |
|-----|--------|
| `Enter` | Execute SQL |
| `Tab` | Switch focus to Shell Input (when completion popup is closed) |
| `Tab` / `Down` | Next completion candidate (when completion popup is open) |
| `Shift+Tab` / `Up` | Previous completion candidate |
| `Ctrl+D` | Execute `SHOW DATABASES` |
| `Ctrl+T` | Execute `SHOW TABLES` |
| `Ctrl+S` | Column selection mode (table → column picker) |

#### Editing

| Key | Action |
|-----|--------|
| `Ctrl+A` | Select all |
| `Ctrl+C` | Copy selection / quit (no selection) |
| `Ctrl+V` | Paste from clipboard |
| `Ctrl+X` | Cut selection |
| `Ctrl+K` | Delete from cursor to end of line |
| `Ctrl+U` | Delete from start of line to cursor |
| `Ctrl+W` | Delete previous word |
| `Ctrl+Y` | Paste from kill buffer |
| `Ctrl+E` | Move cursor to end of line |
| `Home` / `End` | Move cursor to start / end of line |
| `Alt+←` / `Alt+→` | Move cursor one word left / right |
| `Shift+←` / `Shift+→` | Extend selection left / right |
| `Alt+Shift+←` / `Alt+Shift+→` | Extend selection one word left / right |

#### Navigation and Other

| Key | Action |
|-----|--------|
| `Up` / `Down` | Navigate SQL history |
| `ESC` | Clear input / close completion popup |
| `q` (empty input) | Quit |

### Shell Input

Shell Input allows running shell commands without leaving sqsh. Press `Tab` to switch focus from SQL Input.

When connected via a bastion server, commands execute on the bastion host. For direct connections, commands execute locally.

| Key | Action |
|-----|--------|
| `Enter` | Execute command |
| `Tab` | Switch focus to SQL Input |
| `Up` / `Down` | Navigate shell history |
| `Ctrl+A` / `Home` | Move cursor to start |
| `Ctrl+E` / `End` | Move cursor to end |
| `Ctrl+K` | Delete from cursor to end |
| `Ctrl+U` | Delete from start to cursor |
| `Ctrl+W` | Delete previous word |
| `Alt+←` / `Alt+→` | Move cursor one word left / right |
| `ESC` | Clear input |
| `Ctrl+C` | Quit |

### Result Viewer

Result display is handled by skim (Rust-native fzf). Standard fzf key bindings apply.

| Key | Action |
|-----|--------|
| Type to filter | Incremental search |
| `Up` / `Down` | Scroll through results |
| `Enter` | Select record (generates WHERE template) |
| `ESC` / `Ctrl+C` | Return to SQL input |

## Development

```bash
# Run all tests
cargo test

# Run integration tests (requires Docker or Podman)
cargo test --test integration_test
```

## License

MIT
