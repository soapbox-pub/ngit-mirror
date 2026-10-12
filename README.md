# ngit-mirror

Mirrors [ngit](https://ngit.dev) (NIP-34) repositories to GitHub as soon as
their state changes on Nostr.

It watches the repo announcements (kind 30617) of one pubkey and the repo
state events (kind 30618) of each repo's maintainers. When a maintainer
publishes new state, it fetches the commits from the repo's clone URLs into a
local bare cache and force-pushes the branches and tags to
`github.com/<org>/<identifier>`, creating the GitHub repo if needed.

- **The state event is the source of truth**, as in ngit. Only state signed by
  the owner or a listed maintainer (followed transitively through the
  maintainers' own announcements) is used, so nobody else can point the mirror
  at their commits.
- **State often arrives before the commits do.** Syncs that can't find their
  objects yet are retried with backoff, and a newer state replaces a pending
  retry.
- **Only `refs/heads/*` and `refs/tags/*` are mirrored.** The GitHub default
  branch follows the state's `HEAD`.
- **Branches that exist only on GitHub are kept** unless `prune = true`.
- **A full reconcile runs hourly** to catch events missed while relays were
  disconnected.

## Running

```sh
cp config.example.toml config.toml   # then edit
cargo build --release
GITHUB_TOKEN=... ./target/release/ngit-mirror -c config.toml
```

Useful flags:

- `--dry-run` logs what would be created and pushed without touching GitHub.
  Works without credentials for public repos.
- `--once` reconciles every repo a single time and exits.
- `--only <id>` limits it to one repo; repeat it for more.

Log verbosity follows `RUST_LOG` (default `info,nostr_sdk=error`).

Requires `git` on the `PATH`.

## GitHub credentials

These are read from the environment:

- **Personal access token:** `GITHUB_TOKEN`. A classic token needs the `repo`
  and `workflow` scopes.
- **GitHub App (preferred):** `GITHUB_APP_ID`, plus either
  `GITHUB_APP_PRIVATE_KEY` (the PEM itself) or `GITHUB_APP_PRIVATE_KEY_PATH`.
  `GITHUB_APP_INSTALLATION_ID` is optional and looked up from the org if
  unset.
  - Install the app on the org with these repository permissions:
    **Administration: write** (to create repos), **Contents: write**, and
    **Workflows: write**.
  - Without Workflows, any push that touches `.github/workflows` is rejected.

## Deploying

`ngit-mirror.service` is a systemd unit that expects these paths:

- the binary at `/usr/local/bin/ngit-mirror`
- config at `/etc/ngit-mirror/config.toml`
- environment (`GITHUB_APP_ID` or `GITHUB_TOKEN`) in `/etc/ngit-mirror/env`
- a GitHub App private key, if used, at
  `/etc/credstore/ngit-mirror-github-app.pem` (mode 600). The service runs as
  a systemd `DynamicUser`, so it gets the key as a credential rather than
  reading it from disk; leave `GITHUB_APP_PRIVATE_KEY_PATH` unset.

The cache lives in `/var/lib/ngit-mirror`. It can be deleted at any time and
will be refetched.
