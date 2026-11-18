# Install kuroko in a Linear workspace over OAuth

This guide takes you from an unregistered application to a daemon holding a
workspace credential it renews on its own, with no token to paste in later.

You need a workspace where you are an admin, and a callback URL your browser can
reach. The browser returns to that URL during the install, so a daemon only
reachable from its own machine needs a tunnel or a reverse proxy.

## Create the application in Linear

1. Open <https://linear.app/settings/api/applications/new>.
2. Set the callback URL to the daemon's `/linear/oauth/callback` route on a host
   the browser can reach, for example
   `https://kuroko.example/linear/oauth/callback`.
3. Create the application and copy the client ID and the client secret.

The name and icon you pick are how the agent appears in the workspace.

The callback URL is a fixed part of the application. If it does not match the
`redirect_uri` you configure in the next steps, Linear refuses the install.

## Add the four scopes

Linear grants scopes at install time, so nothing is configured on the
application page. kuroko asks for these four scopes in its authorize request:

| Scope | Why kuroko needs it |
| --- | --- |
| `read` | Read the issue a webhook names |
| `write` | Update the issue and post results |
| `app:assignable` | Be delegated an issue |
| `app:mentionable` | Be mentioned in an issue |

The request also sets `actor=app`, which installs the application as its own
workspace user. That user is what the daemon stores and renews, and it needs
admin rights to complete the install.

## Put the two secrets outside the Nix store

The application has two secrets. The client secret authenticates the token
exchange. The install secret is a value only you know, and it guards the install
route so a stranger cannot make your daemon install into their workspace.

Generate the install secret once:

```sh
head -c 32 /dev/urandom | base64
```

Keep both secrets out of any config file the NixOS module renders. That file is
`/etc/kuroko/config.toml`, it points into the Nix store, and every user on the
host can read it. Use one of these places instead:

- The process environment, as `KUROKO_LINEAR_CLIENT_SECRET` and
  `KUROKO_LINEAR_INSTALL_SECRET`.
- A file named by `KUROKO_LINEAR_CLIENT_SECRET_FILE` and
  `KUROKO_LINEAR_INSTALL_SECRET_FILE`. This is how the NixOS module passes them
  through systemd's `LoadCredential`.
- The `environmentFile` of the systemd unit, for the two environment variables.

The value variables win when they hold something, so you can override a stale
credential without editing the unit. A file may end with a newline, which is
trimmed.

Neither secret reaches a log line or a `Debug` rendering, and neither is ever
echoed to the browser.

### On NixOS

Set the two files and the non-secret settings. The module loads the files as
systemd credentials:

```nix
{
  services.kuroko = {
    enable = true;
    environmentFile = "/run/secrets/kuroko.env";
    settings = {
      transport.stdio.command = "claude";
    };
    oauth = {
      clientId = "from_linear";
      redirectUri = "https://kuroko.example/linear/oauth/callback";
      installSecretFile = "/run/secrets/kuroko-install-secret";
      clientSecretFile = "/run/secrets/kuroko-client-secret";
    };
  };
}
```

The module renders `[linear.oauth]` with `client_id` and `redirect_uri`, adds a
`LoadCredential` line for each file, and sets the two `_FILE` variables to the
credential paths. The four `oauth` options are set together or not at all, and
the module refuses to evaluate and names the ones you left out. The unit reads
the files at every start, so restart it after you change one.

### Without NixOS

Write the non-secret keys to the config and supply the secrets in the
environment:

```toml
[linear.oauth]
client_id = "from_linear"
redirect_uri = "https://kuroko.example/linear/oauth/callback"
```

A `client_secret` or `install_secret` written into a config that only you can
read is fine on a plain host. The daemon trims a trailing newline from a secret
it reads from a file, and keeps the whole value it reads from the environment.

## Start the daemon

The daemon must listen on an address the callback URL reaches.

```sh
KUROKO_LINEAR_CLIENT_SECRET=... \
KUROKO_LINEAR_INSTALL_SECRET=... \
nix run .#kuroko -- --config kuroko.toml
```

On NixOS:

```sh
systemctl restart kuroko
```

The daemon stops at boot when `[linear.oauth]` is only partly set, and the error
names every missing key. Without the table the daemon runs exactly as before,
the webhook path works, and both install routes answer `404`.

## Open the install URL

Open the install route on the daemon with the install secret:

```sh
curl -sS -D - -o /dev/null \
  "http://127.0.0.1:8787/linear/install?install_secret=$KUROKO_LINEAR_INSTALL_SECRET"
```

Or open it in a browser and put the secret in the address:

```text
http://127.0.0.1:8787/linear/install?install_secret=<install_secret>
```

A `Location` header pointing at `https://linear.app/oauth/authorize` means the
secret was accepted. A `401` page means it was not. You can send the secret as
`Authorization: Bearer <install_secret>` instead of the query parameter, which
keeps it out of the browser history and out of any proxy log on the way.

The daemon mints a single-use state token that lives ten minutes and sends the
browser to Linear. Approve the application in the workspace. Linear returns the
browser to the callback route, which exchanges the code, reads the workspace's
app user id, and stores the installation in the database.

The state is consumed before the exchange, so a replayed callback gets `401` and
cannot overwrite a saved row. Each HTTP call to Linear gives up after four
seconds, which fails the install without writing a row. A restart mid-install
leaves the state set empty, so start the install again.

## Read the success page

A finished install answers `200` with plain text:

```text
kuroko is installed in Linear.
App user ID: 1a2b3c4d-...
You can close this tab.
```

The app user id is Linear's identifier for this installation in this workspace.
kuroko keys its stored row by that id, so one daemon can hold a row per
workspace.

If the page reports a failure, the status says what to do. `400` means Linear
refused the install or the callback was missing its code or state. `401` means
the state was unknown or expired, so start again. `502` means the exchange with
Linear failed, and the line names the reason. `500` means the row could not be
saved, so check the database.

## Check the installation

```sh
curl -sS http://127.0.0.1:8787/linear/installation
```

A live installation answers `200` with the workspace, the granted scopes, and the
seconds left on the token:

```json
{
  "app_user_id": "1a2b3c4d-...",
  "scopes": ["read", "write", "app:assignable", "app:mentionable"],
  "expires_in": 86399
}
```

The route asks the token provider for a token before it answers, so a `200` also
proves the stored pair still produces one. It never returns a token.

`409` means no installation is stored, or Linear withdrew the grant. The message
says to run the install flow. `404` means `[linear.oauth]` is not configured.
`503` means the installation is there but unusable right now, and the message
carries the reason.

## Reinstall

Run the install flow again from `/linear/install`. The new row replaces the
stored one for the same app user id, and the daemon adopts the new pair without a
restart. Running it after a withdrawal is what clears the withdrawn state.

A reinstall from a different workspace adds a second row. On a cold start the
daemon uses the most recently installed row, so the workspace you installed last
is the one it resumes.

## What happens when the token expires

Linear access tokens last 24 hours by default. The daemon renews the access
token five minutes before it expires, so a call that starts inside that window
cannot outlive the token it was given. The renewal happens when a caller asks for
a token, not on a timer. `GET /linear/installation` is the caller today.

Linear rotates the refresh token on every use, so exactly one caller performs the
exchange and the others wait for the result.

If the renewal request fails for a transient reason, the daemon keeps the stored
pair and answers `503` on the route. Retry it, and the next call renews again. If
the renewed pair cannot be written to the database, the daemon also keeps the old
row and answers `503`. The stored row then holds a refresh token Linear has
already rotated, so check the database before you trust the next renewal.

## What happens when the refresh token is revoked

A refresh token dies when someone revokes the installation in Linear, deletes the
application, or lets the refresh token expire. Linear issues a refresh token with
a 30-day life and resets that life on every use, so an installation that renews
its access token stays live. Linear answers a dead refresh token with
`invalid_grant`, and no retry can fix that.

The daemon marks the grant as withdrawn. `GET /linear/installation` then answers
`409`, and the message says to run the install flow. Every later call fails the
same way until a new install replaces the row. A restart does not clear it,
because the stored row still holds the dead credential.

To recover, run the install flow again from `/linear/install` with the install
secret. A successful install adopts the new pair and clears the withdrawn state,
so the `409` goes away without a restart.

If the client secret itself leaks, rotate it in Linear's application settings and
update the credential file or environment variable. A rotation does not revoke
installed workspaces, so the stored rows keep working with the new secret.
