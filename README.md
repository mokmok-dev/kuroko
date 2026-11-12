# kuroko

kuroko listens for Linear webhooks and hands delegated issues to a local coding agent. It speaks
to that agent over stdio or a websocket, one task at a time, and keeps the task book in memory.

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

[transport.stdio]
command = "claude"
args = ["-p"]
timeout_secs = 3600
```

```toml
[linear]
secret = "whsec_from_linear"

[transport.websocket]
url = "wss://agent.example/ws"
timeout_secs = 3600
```

Exactly one transport table must be present, and a bad value stops the daemon at boot with the
reason. `[server] bind` defaults to `127.0.0.1:8787`, `[linear] replay_window_secs` to 60,
`[dispatch] max_in_flight` to 1, and `[dispatch] max_queued` to 64.

## Point Linear at it

Create a webhook in Linear settings with the URL `http://your-host:8787/webhooks/linear` and pick
the "Issues" resource type. Copy the signing secret into `[linear] secret`. An issue is dispatched
when it is delegated to an agent and `[trigger] any_delegate` is true, or when it carries one of
`[trigger] label_names`. Linear needs a route to the daemon, so use a tunnel or a reachable host.

## Task list

`GET /tasks` returns a JSON array of every task in memory with its key, spec, prompt, and state,
which is `Queued`, `Dispatched`, `Succeeded`, or `Failed`. A terminal state carries the outcome
plus the last bytes of the agent output and log. A restart starts empty.

## Logs

The daemon logs `listening on 127.0.0.1:8787` once the socket is bound, and an `admitted MOK-150`
line for each delivery that became a task. A `delivery dropped` line means the delivery was
genuine but did not trigger, and a `refused` line means the queue was full or the engine was down.
