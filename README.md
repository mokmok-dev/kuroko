# kuroko

kuroko listens for Linear webhooks and hands delegated issues to a local coding agent. It speaks
to that agent over stdio or a websocket, one task at a time, and persists the task book in SQLite
so a restart keeps the queue and every finished result.

## Run it

```sh
nix run .#kuroko -- --config kuroko.toml
```

`--config` defaults to `kuroko.toml` in the working directory. `KUROKO_LINEAR_SECRET` overrides
`[linear] secret` when it is set.

## Config file

```toml
[linear]
secret = "whsec_from_linear"

[store]
path = "kuroko.db"

[transport.stdio]
command = "claude"
args = ["-p"]
timeout_secs = 3600
```

```toml
[linear]
secret = "whsec_from_linear"

[store]
path = "kuroko.db"

[transport.websocket]
url = "wss://agent.example/ws"
timeout_secs = 3600
```

Exactly one transport table must be present, and a bad value stops the daemon at boot with the
reason. `[server] bind` defaults to `127.0.0.1:8787`, `[linear] replay_window_secs` to 60,
`[dispatch] max_in_flight` to 1, `[dispatch] max_queued` to 64, and `[store] path` to
`kuroko.db`. A relative `[store] path` resolves against the directory holding the config file. The
store is opened and loaded before the HTTP listener binds, so a database that cannot be opened
stops the daemon instead of accepting deliveries it cannot persist.

## Point Linear at it

Create a webhook in Linear settings with the URL `http://your-host:8787/webhooks/linear` and pick
the "Issues" resource type. Copy the signing secret into `[linear] secret`. An issue is dispatched
when it is delegated to an agent and `[trigger] any_delegate` is true, or when it carries one of
`[trigger] label_names`. Linear needs a route to the daemon, so use a tunnel or a reachable host.

## Task list

`GET /tasks` returns a JSON array of every task in the book with its key, spec, prompt, and state,
which is `Queued`, `Dispatched`, `RecoveryPending`, `Succeeded`, `Failed`, or `Withdrawn`. A
finished state carries the outcome plus the last bytes of the agent output and log.

## Persistence

The book lives in the database at `[store] path`. A relative path resolves against the directory
holding the config file, and the parent directory must already exist and be writable. Each state
change is written in one transaction: the candidate book is persisted, and only after the commit
does the agent start. A write failure returns `503` and starts nothing, so an agent never runs for
a task the ledger does not know about. A webhook is answered `2xx` only after its admission is
committed.

The store sets `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, and a one-second
`busy_timeout` on every connection. `synchronous=FULL` makes every commit durable before the
webhook is acknowledged, and the one-second lock wait is shorter than the two-second webhook
budget, so one delivery that cannot get the lock is answered `503` instead of committing after
the response. The engine loop serialises commands, though, so deliveries queued behind a
contended store can each spend the busy timeout: an attempt that times out at the router may
still be persisted and dispatched afterwards, and a `503` therefore says "this attempt got no
answer in time", not "nothing was committed". A database that cannot use WAL is refused at boot.
The schema version lives in the `schema_version` table, with one migration step per version; a
database written by a newer binary is refused at boot rather than migrated downward. A row that
does not decode aborts the boot and the error names its `(source, issue, trigger)` key; silently
skipping it would lose an admission record.

`Withdrawn` is a tombstone, not a deletion. Every key stays in the database so a webhook
redelivered after a restart is still a duplicate. Tombstones and terminal rows are retained for
the life of the store, which is what makes admission idempotent and keeps results auditable. The
whole book is rewritten on every state change and `GET /tasks` returns the whole book, so the
per-webhook cost and response size grow with the number of lifetime tasks. That is acceptable for
a single workspace with a modest issue history; a ledger large enough for it to matter needs delta
writes and a pruning policy that keeps at least key-only tombstones.

Only admission is exactly-once. The external agent's side effects are not: a dispatch that was
committed but whose agent result was never recorded comes back as `RecoveryPending`, and kuroko
does not run it again. A delegate-triggered key can be abandoned by removing the delegation in
Linear (which withdraws it). A label-triggered key has no removal webhook, so stop the daemon and
delete its `tasks` row to clear it. A `RecoveryPending` task is visible in `GET /tasks`, does not
occupy an in-flight slot, and keeps its issue serialised until it is resolved. If a completion
cannot be persisted, the daemon exits rather than leaking an in-flight slot; the next start
reconciles that row as `RecoveryPending`.

## Backup

Stop the daemon and copy the database file, or use SQLite's online backup so the WAL is included:

```sh
sqlite3 kuroko.db ".backup 'kuroko-backup.db'"
```

Copying `kuroko.db`, `kuroko.db-wal`, and `kuroko.db-shm` together is also safe while the daemon
is stopped. Restoring is the reverse: put the file back before starting the daemon. A restored
database goes through the same startup reconciliation as a crashed one.

## Startup reconciliation

On boot kuroko loads every task and the queue order, rebuilds the book without emitting a
dispatch, then resumes it: queued tasks with a free slot and an unblocked issue are persisted as
`Dispatched` and started. Every task that was `Dispatched` when the previous process stopped
becomes `RecoveryPending`: the agent it was handed to may still be running, so replaying it would
duplicate side effects, and the resume skips it. Restored queued tasks keep their original order
and the same-issue serialisation rule.

A graceful shutdown lets in-flight agents finish and persists their results. A hard kill leaves
their rows `Dispatched`, which the next start turns into `RecoveryPending`.

## Logs

The daemon logs `listening on 127.0.0.1:8787` once the socket is bound, and an `admitted MOK-150`
line for each delivery that became a task. A `delivery dropped` line means the delivery was
genuine but did not trigger, and a `refused` line means the queue was full, the store write
failed, or the engine was down.
