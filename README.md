# sqsh

A TUI MySQL client for fast database exploration with fuzzy search, written in Rust.

![demo](https://github.com/hatohato25/sqsh/releases/download/v0.1.0/t-rec.gif)

## Features

- **Bastion Support**: Connect to MySQL through SSH bastion servers with per-connection or shared bastion configuration
- **TUI Interface**: Interactive terminal UI powered by ratatui
- **Fuzzy Finder**: Quick connection and table selection with skim (Rust-native fzf)
- **TOML Configuration**: Manage multiple connections in a single config file
- **Connection Pooling**: Configurable connection pool with global defaults and per-connection overrides
- **Read-only Mode**: Prevent accidental writes on sensitive connections
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

SQL input is handled directly by sqsh.

#### Execution and Completion

| Key | Action |
|-----|--------|
| `Enter` | Execute SQL |
| `Tab` / `Down` | Next completion candidate |
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

### Result Viewer

Result display is handled by skim (Rust-native fzf). Standard fzf key bindings apply.

| Key | Action |
|-----|--------|
| Type to filter | Incremental search |
| `Up` / `Down` | Scroll through results |
| `Enter` | Select record (generates WHERE template) |
| `ESC` / `Ctrl+C` | Return to SQL input |

## Local Testing with Docker

The repository ships with a `docker-compose.yml` and initialization scripts under `docker/init/` that spin up a MySQL 8.0 instance pre-loaded with several sample databases.

### Available sample databases

| Database | Description |
|----------|-------------|
| `testdb` | Basic users / products / orders |
| `ecommerce` | Customers, items, transactions, reviews |
| `blog` | Authors, posts, comments, tags |
| `analytics` | Events (~1M rows), sessions, page views |
| `inventory` | Warehouses, products, stock levels, shipments |
| `hr_system` | Departments, employees, projects, time entries |

### Steps

**1. Start the MySQL container**

```bash
docker compose up -d
```

Wait until the container reports healthy (the init scripts run automatically on first start; the `analytics.events` table takes a minute to populate ~1M rows):

```bash
docker compose ps          # STATUS should show "healthy"
```

**2. Create a config file**

```bash
mkdir -p ~/.config/sqsh
cp config.example.toml ~/.config/sqsh/config.toml
chmod 600 ~/.config/sqsh/config.toml
```

Then edit `~/.config/sqsh/config.toml` so the `local-dev` connection points to the Docker instance:

```toml
[[connections]]
name = "local-dev"

[connections.mysql]
host     = "127.0.0.1"
port     = 13306          # mapped port in docker-compose.yml
database = "testdb"
user     = "testuser"
password = "testpass"
ssl_mode = "disabled"
```

**3. Launch sqsh**

```bash
sqsh
```

Select `local-dev` from the connection picker and start exploring.

### Stopping and cleaning up

```bash
docker compose down          # stop containers, keep volume
docker compose down -v       # stop containers and remove volume
```

## Development

```bash
# Run all tests
cargo test

# Run integration tests (requires Docker or Podman)
cargo test --test integration_test
```

## License

MIT
