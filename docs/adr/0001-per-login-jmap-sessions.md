# 1. Per-login JMAP sessions over sharing

Date: 2026-10-03

Status: Accepted

## Context

Docket manages two Fastmail logins: the household login, which we
read and write, and our son's login, which we manage on his behalf. Docket
can reach his mail in either of two ways:

- Separate sessions: his login gets its own API token, and Docket opens
  a second JMAP session with it.
- Sharing: he shares his mail into the household login, and his account
  appears as another `accountId` in the household session. Docket holds
  one token.

Sharing keeps his credential out of Docket, but it depends on how
Fastmail exposes shared accounts over JMAP: whether they show up in the
session, which rights they carry, and whether push covers them. We
haven't tested any of that.

## Decision

Docket uses one credential per Fastmail login and no JMAP sharing. The
household token has the mail and submission scopes. His token has the
mail scope only, so Docket can file his mail but never send as him
(widened from a read-only scope on 2026-10-03, when we decided Docket
should move his messages too).

## Consequences

- Docket treats every credential the same way: one session and one push
  connection each, with accounts read from that session. No code path
  handles shared accounts.
- Docket holds a token for his login. If it leaks, it exposes his mail
  directly rather than through a share we could revoke from the
  household side. The token lives in its own file like any other
  credential (see [Storage](../DESIGN.md#storage)), and a separate file
  is one more secret to provision and rotate.
- Docket derives what it offers from what the server permits, so what
  it can do on each account rests on the credential's scopes, not on
  Docket's config. A scope surfaces in the session as capabilities and
  `isReadOnly` — a read-only token's session advertises mail only and
  the account comes back `isReadOnly: true` (confirmed live,
  2026-10-03). `myRights` is unaffected by scope (it reports mailbox
  ACLs), so it gates per-mailbox actions, not account-wide ones.
- Revisit sharing if separate sessions become a burden, such as more
  monitored logins or token rotation becoming a chore, and test how
  Fastmail exposes shares before switching.
