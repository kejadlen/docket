# Docket — Design

A small self-hosted webapp for two people to jointly manage household email accounts over JMAP.

## Goals

- Make it obvious what needs doing, who's on it, what we're waiting on, and what the other person should see.
- Prevent double-replies and dropped messages without imposing strict ownership.
- Coexist with normal mail clients: the mail server stays the source of truth for mail and folders; Docket is where work gets tracked.
- Support multiple accounts, including one we monitor on our son's behalf.

## Non-goals

- Replacing a full mail client (rich composing, filters, contacts).
- Multi-tenant or public deployment.
- Strict assignment or workflow enforcement.
- Automatic movement. Every change of state, folder, or assignee is made by a person, in Docket or in another client.

## Users and access

- Two app users.
- Access is via Tailscale, which is the whole of authentication: anyone the tailnet lets reach Docket is a user, added on their first request. Docket keeps no list of people.
- The app binds to localhost behind Caddy with [caddy-tailscale](https://github.com/tailscale/caddy-tailscale), which sets two headers from the tailnet identity: `Remote-User` (the full login, `{http.auth.user.id}`) identifies the user, and `X-User-Slug` carries a short name the Caddyfile maps from the login. Requests missing either header are rejected.
- A user's slug is what Docket shows for them. It follows the latest `X-User-Slug` and need not be unique.
- For local work, the `dev` cargo feature stands in for the proxy: a middleware sets both headers for a user picked from a menu. Release builds leave it out, so no setting can turn it on in production.

## Credentials and accounts

Backed by Fastmail. Credentials and accounts are separate:

- A **credential** is a Fastmail API token (JMAP type) for one Fastmail login. It opens a JMAP session.
- An **account** is a JMAP `accountId` from that session.

Chosen setup: **one credential per login, no JMAP sharing.** The household login gets a token with mail + submission scopes; our son's login gets its own token with the mail scope. Sharing his account into the household login was considered and rejected for now; see [ADR 1](adr/0001-per-login-jmap-sessions.md).

**Policy is derived from what the server permits, not config.** The app reads token capabilities, `isReadOnly` on the session account, and `myRights` on each mailbox and only offers actions that are allowed. Token scope surfaces as session capabilities and `isReadOnly` (a read-only token's session advertises mail only with `isReadOnly: true` on the account; `myRights` is unaffected — it reports mailbox ACLs, so it gates per-mailbox actions, not scope).

| | Shared household account | Son's account |
|---|---|---|
| Send / reply | Yes | No (no submission scope) |
| File / archive | Yes | Yes |
| Docket state | Database, mirrored as labels | Database, mirrored as labels |

## Model

The primitive is the **message**. Each received message has three independent values:

- **State**: what's happening with it.
- **Folder**: where it's filed.
- **Assignees**: who it's on.

None of these changes another. A **thread** is a grouping the UI draws from JMAP `threadId` (and headers). Its only data of its own is **comments**. Sent messages have no state, folder, or assignees.

### State

Every received message has exactly one state:

| State | Meaning |
|---|---|
| **Inbox** | Needs someone to look at it. Unassigned means anyone; assigned means those people. |
| **Do** | A real task is on us. |
| **Wait** | Expecting a reply from a third party. |
| **Watch** | Informational, still live (packages, reservations, claims). |
| **Done** | Finished. |

New messages arrive in Inbox. Other messages in the same thread are unaffected — a thread can hold a Done estimate, a Do follow-up, and a new Inbox reply at once.

There is no Read state. "Alex should read this" is Inbox, assigned to Alex. Alex reads it, then moves it on or unassigns himself.

State lives in Docket's database and, on writable accounts, is mirrored as **labels**: mailboxes under a `Docket/` parent (`Docket/Do`, `Docket/Wait`, `Docket/Watch`). Inbox and Done have no label. JMAP mailbox membership is per message, so labels map directly. Labels sit alongside folders — a message can be in `Receipts` and labeled `Docket/Watch` — so state is visible (and editable) in Mail.app and Fastmail web. On read-only accounts, state is database-only.

Per-person read tracking lives in the database — `$seen` is shared across clients and can't say who read something. Opening a message in Docket marks it read for that user. Read tracking drives unread marks only; it never changes state or assignees.

### Assignees

Each message has zero or more assignees (me, them, or both). It's a soft signal, never a lock, and its meaning is the same in every state: these people should deal with this message.

- Unassigned is normal. An unassigned Inbox message is everyone's.
- Assigning someone else is a handoff; add a comment to say why.
- Assignees come off by unassigning themselves. Opening a message doesn't unassign.
- New messages arrive unassigned.

### Folders and filing

Folders are for filing and are managed by Fastmail (including server-side rules); Docket doesn't derive state from them. Any mailbox outside `Docket/` is a folder. Filing is a Docket action on writable accounts only, separate from state:

- Each message is in one folder (or Inbox/Archive). Filing moves the message out of its current folder into the chosen one; `Docket/` labels are untouched.
- Folders are also a filter in Docket views ("Do, in School").

### Comments

Internal comments belong to the thread and appear in the chain in time order, between messages. They never go to the sender and are Docket-only. They replace notes.

### Editing values

There is no toolbar and no thread-level action. Each message's state, folder, and assignees are shown on the message, and clicking a value is how you change it. A change applies to that message only.

## Core flows

- **Landing — For me**: individual messages assigned to me (any state) plus unassigned Inbox messages, grouped by state, then by thread. A thread group lists only its messages in that state; the rest of the thread is omitted.
- **Triage**: from the list or the thread view. Click the value to change it. No modals; an undo toast after each change.
- **Thread view**: received messages, sent messages, and comments in time order, as ruled rows of equal weight. Each row has the sender (and cc/bcc when present) and body on the left; a right gutter holds the date and, for received messages, state, folder, and assignees in fixed positions so they line up down the thread. Sent messages show the recipient after the sender and no values. Comments sit on a filled row. Events ("Sam moved to Do", "filed via another client"). Older messages collapse; their values stay clickable.
- **Lane views**: one list component showing individual messages, grouped by thread (only messages in that lane):
  - Inbox: newest first.
  - Do: oldest first.
  - Wait: oldest first.
  - Watch: compact, by latest update.
  - Done: no list — search only.
- **Accounts**: rows in lists and the thread view carry a visible account marker whenever more than one account is configured — filing choices and reply identity both depend on knowing the account.
- **Reply**: via a normal client for v1; an in-app composer is a fast follow. Docket links out to the thread. A sent reply has no state; moving the message it answers to Wait is a manual step.

## Interop with normal clients

The primary other client is **Mail.app over IMAP**; Fastmail web stays in folders mode. IMAP has no labels, so each `Docket/` mailbox appears as a folder holding its own apparent copy of the message (one message underneath — read state is shared).

- Normal clients can read, file, and reply. In Mail.app, an IMAP move only affects the mailbox being moved out of, so filing Inbox → Receipts keeps the `Docket/` label.
- Triage works from Mail.app by dragging: Inbox → `Docket/Watch` sets the state and removes it from Inbox; option-drag keeps it in both.
- Label changes made elsewhere are user actions, and Docket adopts them rather than reverting: a `Docket/` label added sets that state; a state label removed with no replacement → **Done**; a message moved to Trash → **Done**; an Inbox message removed from Inbox → **Done**. Assignees and comments are Docket-only.
- Accepted cost: filed messages with a state appear in two folders in Mail.app (and likely twice in its search). Collapsing the `Docket` parent in the sidebar hides most of it.
- Changes made elsewhere appear in the thread as "via another client".

## Data and sync

- **Mail server (JMAP)**: messages, folders, and `Docket/` state labels — source of truth for mail and filing.
- **App database**: a cache of threads and messages (filled by JMAP import; fixtures seed it in dev), users, credentials (names matching `docket.kdl`; tokens stay in their files), accounts (credential + `accountId`), per-message state and assignees, per-user read, per-thread comments, history, sync state.
- Records are keyed by account + Message-ID; JMAP ids and `threadId` are cached alongside, since they can change on reimport.
- One JMAP session and push connection (EventSource) per credential; `Email/changes` / `Mailbox/changes` per account, with periodic polling as a fallback.
- Rights are re-read on session refresh.
- No scheduler: nothing changes on a timer.

### Storage

- The app database is **SQLite**: one file beside the app. It fits two users, one process, and one host behind Tailscale, with no database server to run. Back it up by copying the file (or with litestream).
- **Each Fastmail token lives in its own file.** `docket.kdl` names each credential and points at its file (e.g. `credential "household" token-file="/run/credentials/docket/household"`). The config stays free of secrets, so it can be committed and baked into images, and tokens stay out of the environment, where `/proc` and crash dumps can expose them. The file can come from systemd `LoadCredential`, a Docker secret, or sops. The database stores only the credential name, never the token.

## Fast follow

- In-app composer (reply as the shared identity).

## Out of scope for v1

- Automatic movement: reply on Wait → Do, "move to Wait?" suggestions, stale-Watch prompts.
- Dates: follow-up, hidden-until.
- Merging, linking, or splitting threads (a view-level grouping change, since threads hold no values).
- Presence / live "viewing" indicators.
- Push notifications and digests.
- Folder → initial-state automation and filing suggestions.
- Grouping related Watch threads (e.g. by order or tracking number).
- Keyboard shortcuts.

## Open questions

- Should a new message inherit the folder or assignees of the message it replies to? It would be the one exception to "nothing moves automatically".
- Is there a need to mark an Inbox handoff as FYI (no action), or does a comment cover it?
- Whether Fastmail web URLs are stable enough to deep-link to a thread.
- Mail.app behavior test (throwaway message): label it `Docket/Do`, then file, archive, and delete it from Mail.app, checking `mailboxIds` via JMAP after each step. Confirms that moves keep other mailbox memberships and shows what Trash/Archive do to the `Docket/` label.
