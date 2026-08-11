# herdr-devcontainer

A [herdr](https://herdr.dev) plugin that opens panes **inside the repository's
Dev Container** instead of on the host. Point it at a repo that carries a
`.devcontainer/devcontainer.json`, and the plugin brings the container up (via
the official Dev Containers CLI), maps your current directory to the matching
path inside the container, and replaces itself with a `docker exec` into it — so
the pane you get is an ordinary interactive shell or agent session, just on the
other side of the container boundary.

## Requirements

- herdr ≥ 0.8.0
- Docker, with a reachable daemon
- The Dev Containers CLI: `npm install -g @devcontainers/cli`
- A Rust toolchain — the plugin's `[[build]]` hook runs `cargo build --release`
- Linux (the manifest declares `platforms = ["linux"]`)

## Install

From GitHub:

```
herdr plugin install <owner>/<repo>
```

For local development, which skips the build hook — build once yourself first:

```
cargo build --release
herdr plugin link /path/to/herdr-devcontainer
```

## Entrypoints

Three panes, and one action per pane that opens it from anywhere:

| Pane id | Action id | What it does |
|---|---|---|
| `shell` | `open-shell` | Interactive `sh -l` in the container |
| `command` | `open-command` | Runs the configured `command` (default `claude`) |
| `stop` | `open-stop` | Stops the repo's running dev container |

Bind them in your herdr config using the fully qualified action id
(`<plugin id>.<action id>`):

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "devcontainer.open-shell"
description = "dev container shell"

[[keys.command]]
key = "prefix+D"
type = "plugin_action"
command = "devcontainer.open-stop"
description = "stop dev container"
```

## Configuration

Optional, at `$XDG_CONFIG_HOME/herdr-devcontainer/config.toml` (falling back to
`~/.config/herdr-devcontainer/config.toml`):

```toml
command = "claude"          # payload for the "command" pane
up_timeout_secs = 300

[repos."/home/you/workspace/foo"]
enabled = "false"           # "auto" (default) | "true" | "false"
config = ".devcontainer/alt/devcontainer.json"   # repo-relative
```

A missing file means all defaults. Unknown keys are warnings, not errors. Repo
keys are matched against the **canonicalized** repo root, and `config` values are
repo-relative by contract — absolute paths and `..` components are rejected.

A read *failure* is deliberately not treated as a missing file: `NotFound` yields
defaults, but a permission or I/O error is reported as an error, so an unreadable
config can never silently re-enable a repo you set to `enabled = "false"`.

## How it works

Container identity comes from the label the Dev Containers CLI sets itself,
`devcontainer.local_folder=<repo root>`, where the repo root is the **main**
worktree — so every linked worktree of a repo shares one container. If a repo has
more than one *running* match, the plugin refuses to choose and tells you which
ones it found, rather than guessing.

Two rules run through the whole implementation: bring-up is serialized per repo
with an `flock`, so two panes opened at once cannot race; and uncertainty is
never absence — a probe that fails, or a `docker ps` line that will not parse, is
an error, never "there is no container."

The full design, including the wrapper flow step by step, is in
[`docs/superpowers/specs/2026-08-11-herdr-devcontainer-plugin-design.md`](docs/superpowers/specs/2026-08-11-herdr-devcontainer-plugin-design.md).

## Security note

`devcontainer up` executes build hooks defined by the repository —
`postCreateCommand`, `Dockerfile` steps, and so on. Opening a dev container in a
repo you do not trust runs that repo's code, exactly as it does in VS Code Dev
Containers; this plugin inherits that trust model and does not add to it.

The plugin never takes its payload command from repo content: the command run
inside the container comes from your own config file (default `claude`) or is a
plain login shell. Host-side arguments are passed as a direct argv array with no
shell in between, so repository-controlled paths cannot inject host commands.

## Tests

```
cargo test                                  # unit tests
cargo test --test integration -- --ignored  # needs docker, @devcontainers/cli, script(1)
```

Note that nothing in this repo parses `herdr-plugin.toml`, so a green test run
says nothing about the manifest being valid — that is only verified by installing
the plugin into a real herdr and running the entrypoints.
