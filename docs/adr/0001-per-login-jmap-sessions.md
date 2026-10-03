# 1. Per-login JMAP sessions over sharing

Date: 2026-10-03

Status: Accepted

## Context

Docket manages two Fastmail logins: the household login, which we
read and write, and our son's login, which we monitor read-only. Docket
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
household token has the mail and submission scopes. His token has a
read-only mail scope.

## Consequences

- Docket treats every credential the same way: one session and one push
  connection each, with accounts read from that session. No code path
  handles shared accounts.
- Docket holds a token for his login. If it leaks, it exposes his mail
  directly rather than through a share we could revoke from the
  household side. The token lives in its own file like any other
  credential (see [Storage](../DESIGN.md#storage)), and a separate file
  is one more secret to provision and rotate.
- Docket derives what it offers from what the server permits, so the
  read-only guarantee rests on the token's scope, not on Docket's
  config. This assumes Fastmail offers a read-only mail scope, which is
  still unconfirmed. If it doesn't, his token would permit writes and
  Docket would offer them, so revisit this decision.
- Revisit sharing if separate sessions become a burden, such as more
  monitored logins or token rotation becoming a chore, and test how
  Fastmail exposes shares before switching.
