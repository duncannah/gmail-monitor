# gmail-monitor

A small, single-process Rust daemon that waits for Gmail mailbox changes with IMAP
IDLE. It fetches only each new message's IMAP envelope, matches its sender, and
POSTs a JSON webhook. There is no polling loop, message-body download, database,
or async runtime.

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

All configuration uses environment variables:

| Variable | Required | Description |
| --- | --- | --- |
| `GMAIL_EMAIL` | yes | Gmail address used for IMAP authentication |
| `GMAIL_CLIENT_ID` | yes | Google OAuth client ID |
| `GMAIL_CLIENT_SECRET` | yes | Google OAuth client secret |
| `GMAIL_REFRESH_TOKEN` | yes | OAuth offline refresh token |
| `MATCH_SENDERS` | yes | Comma-separated sender addresses; matching is case-insensitive |
| `WEBHOOK_URL` | yes | HTTPS endpoint to POST |
| `WEBHOOK_BEARER_TOKEN` | no | Adds an `Authorization: Bearer ...` header |
| `STATE_PATH` | no | Checkpoint path; defaults to `/data/state.json` |

Webhook body:

```json
{"account":"me@gmail.com","sender":"alerts@example.com","uid":123,"uid_validity":456}
```

The endpoint must return a 2xx status. Otherwise the message remains uncheckpointed
and is retried after reconnecting. Use the `Idempotency-Key` request header when the
webhook performs non-idempotent work.

## Run with Docker

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t gmail-monitor .

docker run --rm \
  --env-file .env \
  -v gmail-monitor-data:/data \
  gmail-monitor
```

BuildKit builds on the selected AMD64 or ARM64 base image. The final image contains
only Debian's TLS runtime, CA certificates, and the stripped daemon binary. It runs
as an unprivileged user.

## Develop with Nix

```sh
nix develop
cargo test
cargo clippy --all-targets -- -D warnings
```

Build the package directly with `nix build`.
