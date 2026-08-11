# herdr-devcontainer plugin — design

- **Date:** 2026-08-11
- **Status:** approved by user (design review in coordinating session)
- **Repo:** `~/workspace/herdr-devcontainer` (fresh project; not part of ProjectMux)

## Goal

A herdr plugin that lets a pane run inside the Dev Container of the repo the user
is working in, using the repo's existing `devcontainer.json` and the Dev
Containers CLI. Target platform: Linux / WSL2 only.

This ports one capability from ProjectMux (`~/workspace/projectmux`, Go): its
per-window `location: host | container` support. The approach is ported, not the
code.

## Non-goals

- No competing container config format — read the repo's `devcontainer.json`,
  delegate everything to the Dev Containers CLI.
- No layered YAML config resolver, no reconciliation, no drift detection, no
  state database.
- No macOS/Windows support.
- No automatic container lifecycle — bring-up and stop are explicit user
  actions. Never `docker rm`.
- No execution of repo-controlled commands with user privileges beyond what
  `devcontainer up` inherently does (build/lifecycle hooks — same trust model as
  VS Code Dev Containers).

## Context and evidence

The executing session will not have the coordinating conversation. Everything
needed is recorded here. Facts below were verified against herdr master
(commit `d004089d`, 2026-08-11, post-v0.8.0) and ProjectMux HEAD.

### herdr plugin surface (verified in herdr source)

- Plugins are directories with a TOML manifest (`herdr-plugin.toml`) plus any
  executables; language is irrelevant to herdr. Manifest entries: `[[build]]`,
  `[[startup]]`, `[[actions]]`, `[[events]]`, `[[panes]]`, `[[link_handlers]]`.
  All `command` values are argv arrays exec'd without a shell.
  (Schema: `src/api/schema/plugins.rs:229-289`; docs `plugins.mdx`.)
- A manifest `[[panes]]` entry's command runs **inside the created pane**, with
  env injected at spawn: `HERDR_PLUGIN_CONTEXT_JSON`, `HERDR_SOCKET_PATH`,
  `HERDR_BIN_PATH`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_ENTRYPOINT_ID`, plus plugin
  root/config/state dirs (`src/app/api/plugins/panes.rs:232-264`).
- `HERDR_PLUGIN_CONTEXT_JSON` is `PluginInvocationContext`
  (`src/api/schema/plugins.rs:363-395`). Relevant fields, all `Option`:
  `focused_pane_cwd`, `workspace_cwd`, and `worktree: WorkspaceWorktreeInfo`
  with `repo_key`, `repo_name`, `repo_root`, `checkout_path`,
  `is_linked_worktree` (`src/api/schema/workspaces.rs:69-75`).
- **`worktree.repo_root` is the parent repo root** (not the worktree checkout):
  production construction at `src/app/worktrees.rs:388-409` sets `repo_root` to
  the source repo root and `checkout_path` to the worktree path. **But**
  `worktree_space` is populated only for workspaces created through herdr's
  worktree flow — the context's `worktree` field is legitimately absent for a
  plain workspace, so the wrapper needs a git-based fallback (see design).
- Manifest pane `placement` defaults to `overlay`; must declare
  `placement = "split"` explicitly. `plugin.pane.open` can override placement,
  cwd, env, size, direction, focus — but **never the command**, which is fixed
  by the manifest (`src/api/schema/plugins.rs:418-439`,
  `src/app/api/plugins/panes.rs`).
- Invocation surfaces: CLI `herdr plugin pane open --plugin ID --entrypoint ID
  [--placement ...] [--cwd ...]` (`src/cli/plugin.rs:483-497,1666`); user
  keybindings can invoke plugin action ids via `[[keys.command]]` with
  `type = "plugin_action"` (configuration docs).
- Popup panes and config-command panes close when their command exits; split
  close-on-exit behavior was not fully verified — the wrapper therefore holds on
  error (prints diagnosis, waits for Enter) so failures can't vanish.
- The socket API has no auth beyond 0600 socket file mode; `layout.apply`
  accepts arbitrary argv per pane. Noted as an alternative only (see
  "Alternatives"), not used by this design.
- Plugin API maturity: v1 shipped in herdr 0.7.0 (2026-06-15); 0.8.0 changed
  plugin registration scope (breaking). Pin `min_herdr_version = "0.8.0"` and
  expect to track breakage.

### ProjectMux semantics being ported (verified in projectmux source)

- **Detection is stat-only**: `.devcontainer/devcontainer.json`, then
  `.devcontainer.json`, or a configured explicit path. Never parse the JSON —
  the Dev Containers CLI is the interpreter. Stat errors other than not-exist
  are errors, not absence (`internal/container/adapter.go:56-81`).
- **Bring-up**: `devcontainer up --workspace-folder <repo-root>
  [--config <path>]`. Synchronous and idempotent; readiness = exit 0 + valid
  JSON. Result JSON is the **last line of stdout** (progress goes to stderr,
  verified on CLI 0.86). Require `outcome == "success"` and non-empty
  `containerId` and `remoteWorkspaceFolder`; `remoteUser` may be empty.
  Default timeout 5 minutes (`internal/container/parse.go:13-49`,
  `adapter.go:157-162`).
- **Exec**: `docker exec -i -t [-u <remoteUser>] -w <workdir> [-e K=V ...]
  <containerId> sh -l` (or `sh -lc <command>`). Workdir = repo-relative cwd
  POSIX-joined (`path.Join` semantics, never host path logic) onto
  `remoteWorkspaceFolder`. Env must be forwarded explicitly with `-e`
  (`internal/container/exec.go:26-42`).
- **Repo-scoped container identity** ("identity collapse", decision 0001):
  always the main repo root, never a linked worktree — per-worktree containers
  failed on gitignored compose overrides and would collide on ports. Discovery
  of existing containers reuses the Dev Containers CLI's own label:
  `docker ps -a --filter label=devcontainer.local_folder=<repo-root>`.
  More than one *running* match is a hard error — refuse to choose
  (`adapter.go:97-144`).
- **Failure-mode details** (expensive to rediscover):
  - Docker error strings drift across versions — match case-insensitively
    (e.g. "no such object"; docker 29 lowercases it).
  - With the daemon down, `docker version --format '{{.Server.Version}}'` on
    some versions exits 0 with empty stdout — reachability requires exit 0
    **and** non-empty stdout (`internal/doctor/dependencies.go:105-110`).
  - `docker stop` needs its own ~30s timeout: Docker's SIGTERM grace is 10s
    before SIGKILL (`adapter.go:201-204`).
  - Uncertainty is never absence: probe failure (daemon down, timeout) must not
    be treated as "no container". Without a state DB this reduces to honest
    error messages rather than repair logic.
  - Subprocess hygiene: spawn children in their own process group; on timeout,
    kill the group (a grandchild holding the pipes otherwise blocks wait);
    bound captured output.

## Decisions (agreed with user)

1. **Mechanism:** manifest-pane wrapper — a `[[panes]]` command that resolves
   the container itself at exec time, inside the new pane. Not the socket
   `layout.apply` route.
2. **Language:** Rust. Single small binary, matches herdr's ecosystem; ~20-30%
   more subprocess plumbing than Go, no design impact.
3. **Container identity:** repo-root scoped, exactly as ProjectMux.
4. **Lifecycle:** up + exec only, plus an explicit stop entrypoint. Never
   automatic stop, never `docker rm`. Stop confirms before acting — see the
   amendment under `herdr-devc stop`.
5. **Trigger discipline:** explicit invocation only (pane entrypoints/actions);
   never auto-triggered from `[[events]]` hooks. Hard rule.
6. **Double-start protection:** `devcontainer up` idempotency + a per-repo flock
   held across bring-up. No state DB; rediscovery via the
   `devcontainer.local_folder` label. The label query runs on **both** paths —
   before bring-up in the pane flow (to refuse an ambiguous repo, per decision 3)
   and in `stop` to pick the container to kill. Leaving it out of the pane flow
   would let `devcontainer up` silently attach to one of several running matches,
   which is exactly the case ProjectMux refuses.
7. **Pane payload:** two variants sharing one wrapper — `shell` (login shell)
   and `command` (configured command, default `claude`).
8. **Placement:** `split` by default (stop is a `popup`).
9. **Config:** one flat TOML at `~/.config/herdr-devcontainer/config.toml`;
   zero-config works. The executed command comes only from this file or the
   built-in default — never from repo content.

## Design

### Shape

- Plugin id `devcontainer`, one Rust binary `herdr-devc`, subcommands `pane`
  and `stop`.
- Manifest structure (syntax to be transcribed precisely from herdr's plugin
  docs at implementation time):

```toml
id = "devcontainer"
min_herdr_version = "0.8.0"

[[build]]
command = ["cargo", "build", "--release"]

[[panes]]
id = "shell"
placement = "split"
command = ["target/release/herdr-devc", "pane", "--shell"]

[[panes]]
id = "command"
placement = "split"
command = ["target/release/herdr-devc", "pane"]

[[panes]]
id = "stop"
placement = "popup"
command = ["target/release/herdr-devc", "stop"]

[[actions]]   # keybindable via [[keys.command]] type = "plugin_action"
id = "open-shell"
command = ["${HERDR_BIN_PATH}", "plugin", "pane", "open",
           "--plugin", "devcontainer", "--entrypoint", "shell"]
# analogous actions for "command" and "stop"
```

Verify at implementation time whether manifest commands expand
`${HERDR_BIN_PATH}`; if not, the action command becomes a tiny wrapper
subcommand (`herdr-devc open shell`) that reads `HERDR_BIN_PATH` from env and
execs the CLI.

**The fallback rests on an unverified assumption**: the env-injection evidence
above (`src/app/api/plugins/panes.rs:232-264`) covers `[[panes]]` commands, and
actions are a different spawn path. If herdr does not inject `HERDR_BIN_PATH`
for `[[actions]]` either, both the primary and the fallback fail and every
keybinding is dead on arrival. Verify the action path specifically — read the
action spawn code and confirm with a live keybinding — **before** building the
fallback around it. If neither path supplies the variable, resolve the herdr
binary another way (PATH lookup for `herdr`, or a configured path).

### Wrapper flow (`herdr-devc pane [--shell]`)

Runs inside the newly created pane; bring-up progress is naturally visible.

1. **Resolve repo root** (fallback chain):
   a. `worktree.repo_root` from `HERDR_PLUGIN_CONTEXT_JSON`;
   b. else derive from `focused_pane_cwd`: `git rev-parse --git-common-dir`,
      main root = parent of the common dir (handle `.git` dirname);
   c. else `workspace_cwd` (same git derivation);
   d. else the wrapper's own process cwd (same git derivation) — herdr sets the
      pane cwd from the focused pane, so this is a meaningful last resort when
      the context JSON is absent entirely;
   e. else fail with guidance ("open a pane in a git repo first").
   Canonicalize the result.
2. **Detection:** per-repo config `enabled = "auto" | "true" | "false"`
   (default auto). Auto stats the configured path or the two standard
   locations; not-exist continues, other stat errors are reported as errors.
   `false` prints "disabled for this repo" and holds. `true` skips the stat and
   lets `devcontainer up` fail on its own if config is missing.
   A configured `config` path must be **repo-relative**: reject absolute paths
   and any `..` component rather than joining them onto the repo root, since
   `Path::join` silently discards the root for an absolute argument.
3. **Preflight:** `devcontainer` on PATH; docker reachable (`docker version
   --format '{{.Server.Version}}'`, require exit 0 AND non-empty stdout). Each
   failure produces a one-line fix hint (install command / start daemon).
4. **Ambiguity check:** `docker ps -a --filter
   label=devcontainer.local_folder=<repo-root>`; if more than one match is
   *running*, list them and fail — never let `devcontainer up` choose. Zero or
   one running match proceeds (bring-up is idempotent and reattaches). Malformed
   `docker ps` output is an error, not absence.
5. **Lock:** flock on
   `$XDG_STATE_HOME/herdr-devcontainer/locks/<sha256-of-canonical-repo-root>.lock`
   (default `~/.local/state/...`), taken blocking with a "waiting for another
   bring-up" note, held only across step 6.
6. **Bring-up:** `devcontainer up --workspace-folder <repo-root>
   [--config <repo-root>/<config-path>]`. stderr inherited (streams to the
   pane), stdout captured (bounded). Timeout `up_timeout_secs` (default 300);
   child in its own process group; on timeout kill the group and report with
   captured tail. Parse last stdout line as JSON; validate as in ProjectMux
   (outcome/containerId/remoteWorkspaceFolder; remoteUser optional).
7. **Workdir mapping:** invocation cwd (the pane's cwd, which herdr sets from
   the focused pane) relative to repo root, POSIX-joined onto
   `remoteWorkspaceFolder`. If cwd is not under the repo root (worktree outside
   the repo, e.g. a sibling checkout), use `remoteWorkspaceFolder` itself and
   print a one-line notice — that content is not mounted in the container.
8. **Exec:** replace the wrapper process (`CommandExt::exec`) with
   `docker exec -i -t [-u <remoteUser>] -w <workdir> <containerId> sh -l`
   (shell variant) or `... sh -lc <command>` (command variant, command from
   config, default `claude`). Host side is direct argv — no host shell, no
   quoting layer; only the container side goes through `sh -lc`.
9. **Any failure:** print classified error + hint, wait for Enter (so the
   message survives regardless of herdr's close-on-exit behavior), exit
   non-zero. Errors that carry captured output (`up` failure, `up` timeout) must
   **print that tail**, not merely store it — an exit code with no diagnostics is
   the failure mode this step exists to prevent.

### `herdr-devc stop`

Popup pane. Resolve repo root (same chain), then
`docker ps -a --filter label=devcontainer.local_folder=<repo-root>` with a
format that captures **id, name, and state** (the spec's "print its id/name"
requires the name be queried, not just the id) →
- no container: say so;
- one running: print its id/name, **confirm**, then `docker stop` with 30s
  timeout, report;
- more than one running: list them and refuse to choose (same rule as
  ProjectMux).
Malformed `docker ps` lines are a hard error, never silently dropped: discarding
an unparseable line turns "I could not tell" into "there is no container", which
violates the uncertainty-is-never-absence rule above.
"No such container" on stop counts as success (already gone). Hold for Enter
before exiting so the popup doesn't vanish with the output.

**Amendment (2026-08-11, after manual verification).** The original design had
stop act immediately, treating decision 5's "explicit invocation only" as
sufficient guard. Manual check 3 showed that guard is weaker than assumed: the
stop keybinding lives one shifted key from herdr's built-in `close_workspace`
(`prefix+shift+d`), which was mis-keyed in testing and closed a workspace, and a
mis-keyed stop is indistinguishable from an intended one while discarding a
running container's state with no undo. Stop therefore prompts
`stop container <id> (<name>) for <root>? [y/N]:` and proceeds only on an
explicit `y`/`yes`. A bare Enter, an unrecognized answer, and an unreadable one
(EOF — closed stdin, non-interactive invocation) all cancel and leave the
container running; this is the uncertainty-is-never-absence rule applied to
consent. The consequence to accept: `herdr-devc stop` is no longer usable
non-interactively without a future opt-out flag, which is the correct default
for a destructive operation with no undo.

### Config — `~/.config/herdr-devcontainer/config.toml`

```toml
command = "claude"          # payload of the "command" pane
up_timeout_secs = 300

[repos."/home/tng/workspace/foo"]
enabled = "false"           # "auto" (default) | "true" | "false"
config = ".devcontainer/alt/devcontainer.json"   # repo-relative
```

Missing file = all defaults. Unknown keys are warnings, not errors. Repo keys
are matched against the canonicalized repo root.

A read failure is **not** the same as a missing file: `NotFound` yields defaults,
while a permission or I/O error is reported as an error. Collapsing the two would
let an unreadable config silently re-enable a repo the user set to
`enabled = "false"`.

`config` values are repo-relative by contract — reject absolute paths and any
`..` component (see wrapper flow step 2).

### Error classification

Distinct, specifically-worded failures for: not a git repo; devcontainer CLI
missing; docker daemon unreachable; config unreadable; no devcontainer config
(auto mode); invalid repo-relative config path; disabled by config; `up` timeout;
`up` failed; `up` output unparseable; malformed `docker ps` output; multiple
running containers. Case-insensitive matching on docker error strings.

`up` failure and `up` timeout carry a captured **stdout** tail. stderr is
inherited so the CLI's progress and error text stream live into the pane; the
captured tail is stdout only, and the error's rendered message must include it.

### Testing

- Unit tests (pure logic, no docker): context JSON parsing and the repo
  fallback chain; up-output parsing (success, garbage, empty, progress on
  stdout, missing fields); workdir mapping incl. worktree outside root; config
  parsing incl. unknown keys, unreadable file, and rejected non-relative
  `config` paths; `docker ps` parsing incl. malformed lines and multiple running
  matches; docker/devcontainer argv construction with hostile paths (spaces,
  quotes).
- Integration tests behind `#[ignore]`: require real docker + devcontainer CLI,
  run manually against a fixture repo with a minimal devcontainer.json. These
  cover `bring_up` and `exec` at the library level and do **not** stand in for
  end-to-end coverage of the pane flow — `run_pane` composes context resolution,
  config, detection, the ambiguity check, locking, and hold-on-error, none of
  which the library-level test touches. Cover that composition with its own
  test that drives the built binary, and verify the manifest wiring by hand.

## Security notes

- `devcontainer up` builds and runs repo-controlled content (Dockerfile,
  lifecycle hooks) — inherent to Dev Containers, same trust model as VS Code.
  Mitigated by trigger discipline: only explicit user invocation, never
  event-driven.
- The host-side process tree never interpolates repo-derived strings into a
  shell; all host spawns are direct argv.
- The command executed inside the container is user-configured
  (config file / built-in default), never read from the repo.

## Risks / open items

- herdr plugin API is ~2 months old and moving; 0.8.x already broke 0.7.x
  installs once. Pin `min_herdr_version`, expect maintenance.
- Split-pane close-on-exit behavior is **verified** (manual check 1, 2026-08-11):
  the hold-on-error path keeps the pane open long enough to read a classified
  error. The mitigation stays; no simplification warranted.
- `HERDR_BIN_PATH` injection on the *action* path remains unverified, and no
  herdr source checkout was available to settle it. Resolved by making the
  variable a preference rather than a requirement: `open::resolve_herdr_bin`
  takes `HERDR_BIN_PATH` when set and non-empty, and otherwise falls back to a
  PATH lookup for `herdr`, so the action path works whether or not herdr injects
  it. No `${...}` expansion is relied on in the manifest.
- **`HERDR_BIN_PATH` can be actively wrong, not merely absent** (found in live
  testing, fixed in `72e998c`). herdr fills it from its own `/proc/self/exe`, so
  upgrading the herdr binary while the server keeps running leaves the literal
  value `/home/u/.local/bin/herdr (deleted)`. Exec'ing that dies with a bare
  `ENOENT` naming nothing actionable. The variable is therefore validated with
  `preflight::is_executable` and an unusable value yields to the PATH lookup —
  deliberately not by stripping the `" (deleted)"` suffix, which would treat the
  symptom. Any future consumer of a herdr-injected path variable should assume
  the same staleness.
- **herdr does not detect keybinding collisions.** `herdr config check` returned
  `config: ok` for a `[[keys.command]]` binding that duplicated a built-in
  default (`prefix+shift+d` = `close_workspace`), and the collision then
  silently shadowed the built-in — observed live, closing a workspace instead of
  opening the plugin pane. Anyone documenting a suggested binding for this
  plugin must diff it against herdr's built-in table by hand
  (`strings $(command -v herdr) | grep -E '^# [a-z_]+ = "prefix\+'`); the
  config checker will not catch it.
- **Stop is confirmed, not immediate** (manual check 3). See the amendment under
  `herdr-devc stop`: the original "explicit invocation is guard enough"
  assumption failed against a keybinding one shifted key from `close_workspace`.
  The cost is that `herdr-devc stop` no longer works non-interactively; add an
  explicit opt-out flag if a scripted caller ever needs one, rather than
  weakening the default.
- Manifest syntax is now **verified against a real herdr 0.8.0**: `herdr plugin
  link` accepts `herdr-plugin.toml` and echoes back every field, and `herdr
  plugin action list` reports all three actions with `platforms` inherited from
  the top level. Placement values are validated by herdr as an enum — `overlay`,
  `popup`, `split`, `tab`, `zoomed` — so the `popup` used by the stop pane is
  valid at manifest level even though `herdr plugin pane open --placement` omits
  it. The re-invocation argv in `open::open_argv` matches the real CLI:
  `herdr plugin pane open --plugin <id> --entrypoint <id>`.

## Alternatives considered

- **Socket `layout.apply`** (arbitrary argv per pane, no manifest constraint):
  rejected as primary — creates a whole new tab rather than a split, depends on
  the currently-unrestricted socket rather than the sanctioned plugin v1
  surface, and hides bring-up progress. Remains available later without rework.
- **`pane.split` + `pane.send_input` typing** (herdr-plus's approach): rejected
  — race-prone (herdr-plus needs deliberate pacing and `pane.read`
  verification), leaves the host shell as the pane's parent process.
- **Go for the wrapper**: equivalent functionally; Rust chosen to match the
  herdr ecosystem at a modest plumbing cost.
