# Docket — Design

A small self-hosted webapp for two people to jointly manage household email accounts over JMAP.

## Goals

- Make it obvious what needs doing, who's on it, and what we're waiting on.
- Prevent double-replies and dropped threads without imposing strict ownership.
- Coexist with normal mail clients: the mail server stays the source of truth for mail, and anything done in another client is picked up.
- Support multiple accounts, including a read-only account we monitor on our son's behalf.

## Non-goals

- Replacing a full mail client (composing rich mail, filters, contacts).
- Multi-tenant or public deployment.
- Strict assignment or workflow enforcement.

## Users and access

- Two app users, one mail credential per account.
- Access is via Tailscale. The app binds to localhost behind `tailscale serve` and identifies users from the `Tailscale-User-Login` header. Requests without a user identity are rejected.
- Each user has a display name and a short slug used in keywords.

## Credentials and accounts

Backed by Fastmail. Credentials and accounts are separate:

- A **credential** is a Fastmail API token (JMAP type, scoped to mail + submission) for one Fastmail login. It opens a JMAP session.
- An **account** is a JMAP `accountId` from that session. A session lists the login's own account plus any accounts shared or delegated to it, so one credential can cover several accounts.

Chosen setup: **one credential per login, no JMAP sharing.** The household login gets a token with mail + submission scopes; our son's login gets its own token with a read-only mail scope. (Alternative considered: sharing his account into the household login, so Docket never holds a credential for his login. Rejected for now in favor of simplicity and no dependency on how Fastmail exposes shares. To be recorded in an ADR — see below.)

**Policy is derived from what the server permits, not config.** The app reads token capabilities, `isReadOnly` on the session account, and `myRights` on each mailbox (`maySetSeen`, `maySetKeywords`, `mayAddItems`, `mayRemoveItems`, …) and only offers actions that are allowed. A read-only token means a bug can't modify his mailbox.

| | Shared household account | Son's account (read-only token) |
|---|---|---|
| Send / reply | Yes | No |
| Write mailboxes, keywords, `$seen` | Yes | No — enforced by the server |
| Status stored in | Mailboxes + keywords | App database only |

Our son's account will eventually be handed over to him. Handoff means revoking Docket's token (and changing his password); since Docket never writes to it, his mailbox carries no trace of the app. Whether our notes and history go with it is an open question.

## Status model

Every thread has exactly one status:

| Status | Meaning | New mail arrives on thread |
|---|---|---|
| **Inbox** | Not triaged yet | Stays |
| **Do** | Something is on us | Stays, marked updated |
| **Wait** | Expecting a reply from a third party | Moves to **Do** |
| **Watch** | Informational, still live (packages, reservations, claims) | Stays, marked updated |
| **Done** | Nothing left to do | Moves to **Inbox** |

Orthogonal to status:

- **Claim** (optional): "mine" / "theirs" — a soft signal, never a lock.
- **Needs ack**: stays flagged until each user has explicitly acknowledged it.
- **Dates**: *follow-up* on Wait (resurfaces as "no response"), *hidden until* on Do (app views only). Watch items with no updates for N days prompt "still watching?"

## Core flows

- **Triage**: move Inbox threads to Do / Wait / Watch / Done in one action from the list; optionally claim or flag for ack.
- **Unified inbox**: one list across accounts with an account badge; filter by account, status, "mine", "needs my ack".
- **Collision avoidance**: show when the other user has a thread open or a draft in progress; banner on threads claimed by the other user.
- **Notes and handoff**: private thread notes that never go to the sender; reassign a claim with a note.
- **Reply** (shared account only): send as the shared identity; after sending, prompt to move the thread to Wait.
- **Watch view**: compact, glanceable list — sender, subject, latest update, age.
- **Catch-up digest**: "since you last looked" summary — new, claimed by the other user, needing your ack, Wait follow-ups due.
- **History**: per-thread audit trail (claimed, moved, replied, noted), including changes made from other clients.

## Interop with normal clients

- **Status = mailbox** on writable accounts: `Inbox`, `Do`, `Wait`, `Watch`, `Archive` (Done). Dragging a message between folders in any client is a status change.
- Status is per-thread; mailboxes are per-message. Changing status moves every non-Sent message in the thread. A thread's status is derived from its newest non-Sent message.
- **Keywords** (namespaced, e.g. `$docket-claimed-<slug>`, `$docket-ack`) carry claim and ack state that other clients can see. Anything private stays in the database.
- **`$seen` is shared** by all clients, so per-user "seen" is tracked in the database.
- Changes made elsewhere are recorded in history as "via another client".
- A reply sent from another client on a Do/Inbox thread triggers a "move to Wait?" suggestion rather than a silent move.

## Data and sync

- **Mail server (JMAP)**: messages, mailboxes, keywords — source of truth for mail.
- **App database**: users, credentials (token references), accounts (credential + `accountId`), thread metadata (dates, claims for read-only accounts), notes, acks, per-user seen, history, sync state.
- Records are keyed by account + root Message-ID; JMAP ids are cached alongside, since they can change on reimport.
- One JMAP session and push connection (EventSource) per credential, covering all its accounts; `Email/changes` / `Mailbox/changes` per account, with periodic polling as a fallback.
- Rights are re-read on session refresh, so a revoked or downgraded share takes effect without config changes.
- Date-driven transitions (follow-ups, stale Watch items) run on a scheduler in the app.

## Open questions

- Done: move to `Archive`, or treat any non-status folder as Done to preserve existing filing?
- Notifications: channel (push, email digest, both) and per-user rules.
- Son's account at handoff: export, delete, or keep our notes and history?
- Grouping related Watch threads (e.g. by order or tracking number) — later.
- Stack and database choice.
- Confirm Fastmail offers a read-only mail scope for API tokens, and how it shows up in the session (`isReadOnly`, missing capabilities, or write errors).
- Token storage: where API tokens live on the host (env, file, secret store).

## When development starts

- Write an ADR recording the choice of separate per-login sessions over JMAP sharing for our son's account, including the trade-offs: simplicity and no sharing unknowns vs. Docket holding a credential for his login.