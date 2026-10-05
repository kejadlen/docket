# Docket

Docket is a small self-hosted web app for two people to triage shared
household email together. Each message gets a state (Inbox, Do, Wait,
Watch, or Done), a folder, and assignees, so it's clear what needs doing
and who's on it. The mail server stays the source of truth for mail, and
normal mail clients keep working alongside Docket.

Docket is early. It imports Inbox mail from Fastmail over JMAP and polls
for changes every 30 seconds, but it doesn't write anything back to the
mail server yet, so states, folders, and assignees live only in Docket's
database. A demo built from `main` runs on sample fixture data at
<https://docket.demo.kejadlen.dev>. See [docs/DESIGN.md](docs/DESIGN.md)
for the model and the plan.

## Run it locally

You need a Rust toolchain, [just](https://github.com/casey/just), and,
for `just dev`, [fd](https://github.com/sharkdp/fd) and
[entr](https://eradman.com/entrproject/).

```sh
just dev
```

This serves the UI on <http://127.0.0.1:3000> and restarts on changes.
It builds with the `dev` feature, which seeds an empty database with
fixtures and lets you pick a user from a menu instead of signing in
through Tailscale. Delete `docket.db` to start over from the fixtures.

## Configure it

Docket reads its settings from `docket.kdl` in the working directory, or
from the file named by `--config` or `DOCKET_CONFIG`:

```kdl
bind "127.0.0.1:3000"
database "docket.db"
log warn
credential "household" token-file="/run/credentials/docket/household"
```

| Setting | Default | Meaning |
|---|---|---|
| `bind` | `127.0.0.1:3000` | Address to listen on. |
| `database` | `docket.db` | SQLite database file, created if missing. |
| `log` | `warn` | Log level for the app, with optional per-target overrides as properties: `log debug hyper=warn`. |
| `credential` | None | A Fastmail API token to sync mail with. Repeat it once per login. |

Each `credential` takes a name, which becomes the account's slug, and a
`token-file` holding the token, which keeps the token out of the config.
Docket opens a JMAP session for every credential at startup and exits if
any of them fails. With no credentials, it syncs no mail.

## Deploy it

Docket has no sign-in of its own. It expects to sit on localhost behind
Caddy with [caddy-tailscale](https://github.com/tailscale/caddy-tailscale),
which sets two headers on every request:

- `Remote-User`: the user's full Tailscale login, which identifies them.
- `X-User-Slug`: a short display name, mapped from the login in the
  Caddyfile.

Docket rejects requests missing either header. Anyone the tailnet lets
through becomes a user on their first request.

Pushes to `main` publish a release image to
`ghcr.io/kejadlen/docket`. The release build leaves out the `dev`
feature, so nothing in its config can turn on the fixture data or the
stand-in sign-in.

## Develop

```sh
just        # format, clippy, and tests with coverage
just mutants
```

The coverage check fails below 100% line coverage of the library.
Set `COVERAGE_THRESHOLD` to override it.

CI runs `cargo fmt --check` and `just clippy coverage` on every pull
request, and each pull request gets its own Fly preview app.
