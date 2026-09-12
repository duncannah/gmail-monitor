# gmail-monitor

A small, single-process Rust daemon that waits for Gmail mailbox changes with IMAP
IDLE. It fetches each new message's IMAP envelope, checks independently configured
webhook routes, and only downloads the message body when a route matches. There is
no polling loop, database, or async runtime.

On its first run, the daemon checkpoints the current end of the inbox and only
handles mail that arrives afterward. It persists `UIDVALIDITY` and the last handled
UID in one small JSON file. A webhook receives an `Idempotency-Key` header so it can
also deduplicate the rare case where delivery succeeds but the daemon stops before
writing its checkpoint.

## Gmail OAuth setup

1. In Google Cloud Console, create or select a project.
2. Configure the OAuth consent screen and add the Gmail account as a test user if
   the app is in testing mode.
3. Create an OAuth client (Desktop app is convenient for a personal deployment).
4. Complete Google's authorization-code flow once with the scope
   `https://mail.google.com/` and request offline access. Save the resulting refresh
   token. Google's OAuth 2.0 Playground can be used for this; enable "Use your own
   OAuth credentials" before authorizing.
5. Ensure IMAP access is allowed for the Google Workspace account. Personal Gmail
   accounts have IMAP access enabled by default.

The refresh token is exchanged for a short-lived access token whenever the daemon
connects or reconnects. It is never written to disk.

## Configuration

Copy [`config.example.toml`](config.example.toml) and define one or more webhook
routes:

```toml
state_path = "/data/state.json"

[gmail]
email = "me@gmail.com"
client_id = "your-client-id.apps.googleusercontent.com"
client_secret = "your-client-secret"
refresh_token = "your-refresh-token"

[[webhooks]]
name = "alerts"
url = "https://example.com/hooks/alerts"
senders = ["alerts@example.com", "status@example.net"]
bearer_token = "optional-token"

[[webhooks]]
name = "billing"
url = "https://example.com/hooks/billing"
senders = ["billing@example.com"]
```

Webhook names must be unique. Sender matching is case-insensitive. The
`bearer_token` and top-level `state_path` fields are optional; the state path
defaults to `/data/state.json`.

Set `CONFIG_PATH` to change the configuration path. It defaults to
`/config/config.toml`, which works well with a read-only Docker bind mount.

Webhook body:

```json
{
  "account": "me@gmail.com",
  "sender": "alerts@example.com",
  "body": "This is the message body.\r\n",
  "uid": 123,
  "uid_validity": 456
}
```

The `body` field contains Gmail's IMAP `BODY[TEXT]` value. For MIME messages, it can
contain MIME boundaries and transfer-encoded content. The daemon does not download
the body at all when no route matches.

The endpoint must return a 2xx status. Otherwise the message remains
uncheckpointed and is retried after reconnecting. Each route gets a stable
`Idempotency-Key` header in the form `gmail:<route>:<uid_validity>:<uid>`.

## Run with Docker

Pull the published ARM64 image from GitHub Container Registry:

```sh
docker pull ghcr.io/duncannah/gmail-monitor:latest

docker run --rm \
  -v ./config.toml:/config/config.toml:ro \
  -v gmail-monitor-data:/data \
  ghcr.io/duncannah/gmail-monitor:latest
```

The image targets ARM64. It contains only Debian's TLS runtime, CA certificates,
and the stripped daemon binary, and runs as an unprivileged user.

To build it locally instead:

```sh
docker build -t gmail-monitor .
```

## Develop with Nix

```sh
nix develop
cargo test
cargo clippy --all-targets -- -D warnings
```

Build the package directly with `nix build`.
