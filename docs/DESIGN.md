# Docket — Design

A small self-hosted webapp for two people to jointly manage household email accounts over JMAP.

## Goals

- Make it obvious what needs doing, who's on it, what we're waiting on, and what the other person should see.
- Prevent double-replies and dropped threads without imposing strict ownership.
- Coexist with normal mail clients: the mail server stays the source of truth for mail and folders; Docket is where work gets tracked.
- Support multiple accounts, including a read-only account we monitor on our son's behalf.

## Non-goals

- Replacing a full mail client (rich composing, filters, contacts).
- Multi-tenant or public deployment.
- Strict assignment or workflow enforcement.

## Users and access

- Two app users.
- Access is via Tailscale. The app binds to localhost behind `tailscale serve` and identifies users from the `Tailscale-User-Login` header. Requests without a user identity are rejected.
- Each user has a display name and a short slug.

## Credentials and accounts

Backed by Fastmail. Credentials and accounts are separate:

- A **credential** is a Fastmail API token (JMAP type) for one Fastmail login. It opens a JMAP session.
- An **account** is a JMAP `accountId` from that session.

Chosen setup: **one credential per login, no JMAP sharing.** The household login gets a token with mail + submission scopes; our son's login gets its own token with a read-only mail scope. (Alternative considered: sharing his account into the household login so Docket never holds a credential for his login. Rejected for now in favor of simplicity and no dependency on how Fastmail exposes shares. To be recorded in an ADR — see below.)

**Policy is derived from what the server permits, not config.** The app reads token capabilities, `isReadOnly` on the session account, and `myRights` on each mailbox and only offers actions that are allowed.

| | Shared household account | Son's account (read-only token) |
|---|---|---|
| Send / reply | Yes | No |
| File / archive | Yes | No |
| Docket state | Database, mirrored as labels | Database only |

## Model

Three independent concepts per thread: **state** (what's happening), **assignment** (who it's on), and **folder** (where it's filed).

### State

State lives in Docket's database and, on writable accounts, is mirrored as **labels**: mailboxes under a `Docket/` parent (`Docket/Read`, `Docket/Do`, `Docket/Wait`, `Docket/Watch`). Inbox and Done have no label. Labels sit alongside folders — a message can be in `Receipts` and labeled `Docket/Watch` — so state is visible (and editable) in Mail.app and Fastmail web. On read-only accounts, state is database-only.

Every thread has exactly one state:

| State | Meaning | Leaves when | New mail arrives |
|---|---|---|---|
| **Inbox** | Not triaged | Triaged | Stays |
| **Read** | Someone needs to read it | All assigned readers have read it → destination chosen at triage (Done by default, or Do / Watch) | Stays, marked updated |
| **Do** | A real task is on us | Acted on → Wait / Done | Stays, marked updated |
| **Wait** | Expecting a reply from a third party | Reply arrives | Moves to **Do** |
| **Watch** | Informational, still live (packages, reservations, claims) | Marked done, or prompted after going stale | Stays, marked updated |
| **Done** | Finished | New mail arrives | Moves to **Inbox** |

Dates: *follow-up* on Wait (resurfaces as "no reply in N days"), *hidden until* on Do. Watch items with no updates for N days prompt "still watching?"

Read state is per person and tracked in the database — `$seen` is shared across clients and can't say who read something. Opening a thread in Docket marks it read for that user.

### Assignment

Each thread has zero or more assignees (me, them, or both). It's a soft signal, never a lock, and its meaning follows the state:

- **Read**: who still needs to read it. Readers drop off as they read; when none remain, the thread moves on.
- **Do**: who's handling it. Unassigned is fine.
- **Wait**: who's following up — defaults to whoever moved it to Wait. When a reply moves it back to Do, it stays assigned to them.
- **Watch / Done**: none.

Assigning to yourself replaces the earlier "claim". Assigning to the other person is a handoff and can carry a note ("can you call them?"). Anything newly assigned or shared with you is pinned at the top of your Inbox until you open it.

### Folders and filing

Folders are for filing and are managed by Fastmail (including server-side rules); Docket doesn't derive state from them. Any mailbox outside `Docket/` is a folder. Filing is a Docket action alongside state changes, on writable accounts only:

- The triage sheet has an optional **File to…** picker next to the state choice (e.g. `Done → Receipts`, `Watch → Shipping`).
- **Done** without a folder archives (removes from Inbox). Docket never touches filing folders except when you file.
- Filing moves the whole thread: every non-Sent message leaves its current folder (usually Inbox) and is added to the chosen one. `Docket/` labels are left untouched.
- Threads remember their last folder; when new mail brings a filed thread back, the picker preselects it.
- Folders are also a filter in Docket views ("Do, in School").

## Core flows

- **Landing**: the Inbox, with a pinned "For you" section (assigned or shared to you, not yet opened) and a strip of counts — `Read 2 · Do 4 (2 mine) · Wait 1 overdue · Watch 3 updated`. If Inbox is empty, land on Do.
- **Triage**: from the list without opening the thread (swipe/tap to a sheet) or from the thread view. Pick state, optionally assignee and folder. No modals; an undo toast offers quick follow-ups ("+ follow-up 3d", "assign to me").
- **Thread view**: header with account, state, assignees, folder. A single timeline interleaves messages, notes, and events ("moved to Wait by A", "archived via another client"). Older messages collapse.
- **Lane views**: one list component with per-lane grouping and sort:
  - Read: "you need to read" vs. "waiting on them to read".
  - Do: grouped Mine / Unassigned / Theirs, oldest first; hidden-until under "Later".
  - Wait: by follow-up date, overdue pinned.
  - Watch: compact, by latest update; stale items get a "still watching?" row.
  - Done: no list — search only.
- **Notes and handoff**: private thread notes that never go to the sender; reassigning with a note.
- **Reply**: via a normal client for v1; an in-app composer is a fast follow. Docket links out to the thread and, on seeing a sent reply, suggests moving to Wait.

## Interop with normal clients

The primary other client is **Mail.app over IMAP**; Fastmail web stays in folders mode. IMAP has no labels, so each `Docket/` mailbox appears as a folder holding its own apparent copy of the message (one message underneath — read state is shared).

- Normal clients can read, file, and reply. In Mail.app, an IMAP move only affects the mailbox being moved out of, so filing Inbox → Receipts keeps the `Docket/` label.
- Triage works from Mail.app by dragging: Inbox → `Docket/Watch` sets the state and removes it from Inbox; option-drag keeps it in both.
- Adding or removing a `Docket/` label elsewhere changes state: Docket adopts it rather than reverting it. A state label removed with no replacement → **Done**. A message moved to Trash → **Done**. Assignees, dates, and notes are Docket-only.
- Accepted cost: filed messages with a state appear in two folders in Mail.app (and likely twice in its search). Collapsing the `Docket` parent in the sidebar hides most of it.
- Docket applies state labels to every non-Sent message in a thread, including new arrivals.
- A reply sent elsewhere on a Do/Inbox thread triggers a "move to Wait?" suggestion rather than a silent change.
- A thread that is still untriaged and gets removed from Inbox elsewhere is treated as Done. Threads in any other state keep their state.
- Changes made elsewhere appear in the thread timeline as "via another client".

## Data and sync

- **Mail server (JMAP)**: messages, folders, and `Docket/` state labels — source of truth for mail and filing.
- **App database**: users, credentials (token references), accounts (credential + `accountId`), thread state, assignees, dates, destination-after-read, last folder, per-user read, notes, history, sync state.
- Records are keyed by account + root Message-ID; JMAP ids are cached alongside, since they can change on reimport.
- One JMAP session and push connection (EventSource) per credential; `Email/changes` / `Mailbox/changes` per account, with periodic polling as a fallback.
- Rights are re-read on session refresh.
- Date-driven transitions (follow-ups, hidden-until, stale Watch items) run on a scheduler in the app.

## Fast follow

- In-app composer (reply as the shared identity).

## Out of scope for v1

- Presence / live "viewing" indicators.
- Push notifications and digests.
- Folder → initial-state automation and filing suggestions.
- Grouping related Watch threads (e.g. by order or tracking number).
- Keyboard shortcuts.

## Open questions

- Whether Fastmail web URLs are stable enough to deep-link to a thread.
- Mail.app behavior test (throwaway message): label it `Docket/Do`, then file, archive, and delete it from Mail.app, checking `mailboxIds` via JMAP after each step. Confirms that moves keep other mailbox memberships and shows what Trash/Archive do to the `Docket/` label.
- Confirm Fastmail offers a read-only mail scope for API tokens, and how it shows up in the session.
- Token storage on the host (env, file, secret store).
- Stack and database choice.

## When development starts

- Write an ADR recording the choice of separate per-login sessions over JMAP sharing for our son's account, including the trade-offs: simplicity and no sharing unknowns vs. Docket holding a credential for his login.