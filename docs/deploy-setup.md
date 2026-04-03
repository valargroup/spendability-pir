# Deploy setup for spend-server

This guide covers two deployment paths:

- **[Binary setup](#binary-setup-operators)** — Download a pre-built binary and run the service. No Rust toolchain or git clone required.
- **[Source setup](#source-setup-developers)** — Build from source with CI/CD-driven deployment.

---

## Hardware requirements

| Resource | Minimum | Recommended | Notes |
|----------|---------|-------------|-------|
| **CPU** | x86-64 (any) | x86-64 or ARM64 | Any modern CPU. No AVX-512 requirement. |
| **RAM** | 8 GB | 16 GB | The server loads ~56 MB of PIR data and builds YPIR internal structures. Peak usage during initialization is higher. |
| **Disk** | 10 GB free | 20 GB free | Snapshot data, PIR database, plus headroom. |
| **OS** | Linux (x86-64) | Ubuntu 22.04+ / Debian 12+ | macOS (arm64/amd64) binaries are also published but not recommended for production serving. |
| **Network** | Outbound HTTPS | Static IP or DNS A record | Needs outbound access to a lightwalletd gRPC endpoint for syncing. Inbound access on the serve port for wallet clients. |

---

## Binary setup (operators)

This path is for operators who want to run `spend-server` without cloning the repository or installing the Rust toolchain.

### 1. Download the binary

Grab the latest release from GitHub:

```bash
# Pick the asset for your platform
PLATFORM="linux-amd64"   # or: linux-arm64, darwin-arm64
VERSION=$(curl -s https://api.github.com/repos/valargroup/sync-nullifier-pir/releases/latest | grep tag_name | cut -d'"' -f4)

sudo mkdir -p /opt/spend-server
cd /opt/spend-server

# Download the binary and systemd unit
curl -fLO "https://github.com/valargroup/sync-nullifier-pir/releases/download/${VERSION}/spend-server-${PLATFORM}"
curl -fLO "https://github.com/valargroup/sync-nullifier-pir/releases/download/${VERSION}/spend-server.service"

sudo mv "spend-server-${PLATFORM}" spend-server
sudo chmod +x spend-server
```

### 2. Create the data directory

```bash
sudo mkdir -p /opt/spend-server/data
```

### 3. Install the systemd service

```bash
sudo cp /opt/spend-server/spend-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable spend-server
sudo systemctl start spend-server
```

Verify the service is running:

```bash
sudo systemctl status spend-server
curl http://localhost:8080/health
```

The server will automatically connect to lightwalletd, sync recent blocks, build the YPIR database, and start serving queries. During sync, `GET /health` reports progress and `POST /query` returns 503.

---

## Source setup (developers)

This path is for contributors and operators who want to build from source with CI/CD-driven deployment.

### GitHub repository secrets

In the repo: **Settings -> Secrets and variables -> Actions**, add:

| Secret | Description |
|--------|-------------|
| `DEPLOY_HOST` | Remote hostname or IP (e.g. `pir.example.com` or `192.0.2.10`). |
| `DEPLOY_USER` | SSH user on that host (e.g. `deploy` or `ubuntu`). |
| `SSH_PASSWORD` | SSH password for that user. |

### One-time setup on the remote host

**Directory and permissions**

- Create the deploy directory: `sudo mkdir -p /opt/spend-server/data`
- Ensure the SSH user can write to that directory.

**Systemd service**

The `spend-server` binary is an all-in-one server that syncs from lightwalletd and serves PIR queries. It needs:

- **lightwalletd endpoint**: Configured via `--lwd-url` (default in the service file: `https://zec.rocks:443`).
- **Data directory**: For snapshots and hint cache, configured via `--data-dir`.
- **Port**: Configurable via `--listen` (default `0.0.0.0:8080`).

A systemd unit file is provided at `docs/spend-server.service`. Copy to `/etc/systemd/system/`:

```bash
sudo cp /opt/spend-server/spend-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable spend-server
sudo systemctl start spend-server
```

### Changing deploy path or restart command

- **Deploy path**: Edit the `env.DEPLOY_PATH` in `.github/workflows/deploy.yml` (default `/opt/spend-server`).
- **Restart command**: Edit the "Install config and restart services" step in that workflow if you use a different service name.

### Manual runs

`deploy.yml` supports `workflow_dispatch`, so you can trigger it from **Actions -> Run workflow** without pushing to `main`.

### Test locally

From the workspace root:

```bash
# Build the server binary
make build

# Run with default settings
make run

# Or run with custom lightwalletd endpoint
make run LWD_URL=http://localhost:9067
```

Then check `http://localhost:8080/health`.

---

## CI/CD workflows

The workflows in `.github/workflows/` handle building and deploying `spend-server`:

- **`ci.yml`** — Runs format checks, clippy, and tests on every push/PR to `main`.
- **`deploy.yml`** — Builds on every push to `main` and deploys to a remote host via SSH.
- **`release.yml`** — Builds multi-platform binaries and publishes a GitHub Release on version tags.
