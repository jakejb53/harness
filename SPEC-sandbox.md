# Sandboxed REPL

Replaces the CPython REPL with Monty (`monty-pool` 1.0.0), a Python-subset interpreter with no filesystem, environment, network, or process access of its own. Everything outside the sandbox goes through the app.

Every error, denial, and failure the app reports to the model states what happened, why, and any consequence for the session (such as lost state). Messages below are the standard.

## Config

Removed: `[classifier]`, `[repl] approval_mode`. Added:

```toml
[repl]
tool_description = "..."     # REPL tool description, sent verbatim
permission = "none"          # "none" | "read-only" | "read-write"; rewritten by Shift+Tab
max_memory_mb = 2048         # REPL worker memory limit in MiB

[commands]
deny = ["git", "rm"]         # command names; always denied
env_filter = ["GITHUB_TOKEN"] # exact variable names removed from commands' environment

[commands.allow]             # name the model calls = absolute path that runs
cargo = "/Users/me/.cargo/bin/cargo"
```

`tool_description`, `permission`, and `max_memory_mb` are required; omission is a startup error. `tool_description` is loaded at startup and used verbatim in every REPL tool definition. The values above are examples, not runtime defaults. An `allow` value that isn't an absolute path is a config error. A name in both `deny` and `allow` is denied.

Optional `[approval_description]`: required `enabled` boolean; when true, require a `prompt` containing `${command}` and `[approval_description.endpoint]` with `base_url`, `model`, optional `api_key_env`, and optional `parameters` as for the chat endpoint. An absent section or false toggle disables requests. Endpoints resolve credentials independently, including shared `.env` key names.

## REPL

- Monty worker: the harness binary run as a hidden worker subcommand (`monty_proto::worker::Child` over stdio, `monty_alloc::LimitedAllocator` as global allocator), launched via `current_exe()` by a `monty_pool::Pool` with one idle worker and at most one worker.
- A turn checks out a worker (fresh session), feeds `replib/*.py`, then runs the turn's calls; state persists across the turn's calls. The turn's end finishes the session and returns the worker. Esc or exit drops the checkout, discarding the worker. A crashed worker ends the call with `[REPL worker crashed: <reason>. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`; the next call gets that new session.
- Static type checking is disabled. Type annotations are not enforced; runtime errors stop execution where they occur. Completed effects and surviving REPL state remain available to later calls after ordinary exceptions. Host functions validate arguments and enforce permissions at each operation.
- Limits: `max_suspensions = usize::MAX`; `max_memory` from `[repl] max_memory_mb`; no time limits (Esc stops). Hitting the memory limit discards the worker and ends the call with `[REPL memory limit of <N> MiB exceeded. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`.
- The repo is mounted read-write at the virtual path `/repository`, which is also the sandbox working directory; its host path is never visible to sandbox code. The directory handle is opened once and reused across feeds and turns. `os.getenv`/`os.environ` see an empty environment.
- Result: `print` output in order, then the repr of a trailing expression if not `None`, then the exception type and descriptive message without traceback frames if the code raised; `(no output)` if empty; truncated at `output_limit` as before. Library-load exceptions and unsupported-operation records use the same exception summary.
- Except for file injection below, all synthetic tool calls, present and future, including the per-turn `FYI()`, never require approval at any permission level, whether app-generated or explicitly called by the model. `help()` runs only on demand and also requires no approval. Unrelated code bundled with them, including code evaluated in arguments, remains subject to normal permission and approval rules. App-generated file injections follow the separate user-authorized read contract below.
- `reasoning()` is not synthetic and remains separately approval-exempt at every permission level. Exact `reasoning(<string literal>)` calls are answered `(no output)` without running; unrelated code cannot inherit the exemption.

## Permissions

Every REPL file operation reaches the app as an OS call and is checked before it is serviced:

- Inside the repo: `none` denies all; `read-only` denies writes; `read-write` allows all.
- Filesystem operations are serviced exclusively by the repository mount. Outside access is denied at every level, with no outside-path approvals or host filesystem handlers.
- Unmodified Monty enforces containment against traversal and symlink escapes. Absolute paths and `..` components are not rejected solely for their spelling when the mount contains the operation. Monty rejects absolute symlink targets even inside the repo; filesystem predicates can return `False` for rejected paths. `Path.resolve()` uses Monty's lexical normalization, not host symlink resolution.
- A change of level applies from the next operation, including mid-call.
- App denials raise `PermissionError` at the call, e.g. `write /repo/x: denied: the permission level is read-only`, `read /etc/hosts: denied: the path is outside the repository`, `run 'git': denied: 'git' is on the command deny list`.

The direct `apply_patch` tool uses an independent `cap-std` repository directory capability, not Monty. It requires `read-write` and rejects all absolute paths. Path resolution cannot escape the repository or follow absolute symlink targets. Text updates follow relative symlinks within the repository; deletion and rename operate on named entries. Permission revocation stops subsequent patch operations; already committed changes remain reported. Patch behavior is specified in `SPEC.md`.

User file injection (`@path`, specified in `SPEC.md`) is an app-side read outside Monty, authorized independently of these permissions, including `none` and paths outside the repository. The app records Python file-reading code as a synthetic `REPL` call with the complete UTF-8 file as its result, including empty contents, without execution or `output_limit` truncation. Injection saves only these pairs: the shared encoded call ID carries the injected-file kind, and the filename stays in the code. There is no separate tracking field, turn, user message, `FYI()`/`help()` execution, or model request. Attachment command lines never enter the prompt or history; successful loading removes them from the draft and leaves any ordinary text unsent for a separate Enter. This exception belongs to that user-selected instance; identical code requested by the model retains normal REPL permissions and limits. Attachment failures or cancellation preserve the original full draft and history. Recognized injection instances persist across turns and resume until individually removed with confirmation; `FYI()`/`help()` cleanup does not remove them. The ID format is obfuscation, not an authorization or confidentiality mechanism; newly received provider calls receive normal IDs before saving.

## Commands

`run(argv, cwd=None)` → `{"exit_code": int, "stdout": str, "stderr": str}`; prints nothing. A command killed by a signal has `exit_code` = minus the signal number.

- `argv[0]` is a command name, or a relative path containing `/` (no `..`) to an executable inside the repo, resolved against `cwd` through symlinks. Outside the repo: denied. Missing: `FileNotFoundError`. Not a regular file with an execute bit: `PermissionError: run 'scripts/build.sh': not an executable file`, not logged as a denial. A path skips `deny` and `allow` and always asks `allow <path> <args>?` with the resolved path; errors name the path as spelled.
- In `deny`: denied. In `allow`: runs the configured path without asking. Otherwise the app resolves the name through its own `PATH` and asks `allow <path> <args>?`; not found raises `FileNotFoundError: run: no command named 'npm' is configured or found on the host`. Independent of the permission level.
- `cwd` defaults to the repo root. An explicit `cwd` must be relative with no `..` components and resolves against the repo root, independently of sandbox `os.chdir()`; if symlink resolution places the directory outside the repo, it asks.
- The program runs directly (no shell), stdin `/dev/null`, in its own session with no controlling terminal, with the app's environment minus `env_filter`. Commands are host processes, not confined by the REPL mount. Esc kills the process group. No timeout. Output is decoded as UTF-8 with replacement and never shown live.

## Library

- `~/.harness/replib/*.py` is Monty code, fed in name order at session start. Load errors are printed in the first call's output. The repo ships `reasoning.py`, `glob.py`, and `search.py`.
- Public library functions are the top-level functions whose names don't start with `_`. Monty functions expose no name, signature, or docstring, so the app reads these from the source with `ruff_python_parser`.
- `lib_source(name)`: returns the contents of `replib/<name>.py`.
- `glob` and `search` return relative string paths preserving the supplied root (default `.`), skip only the entry names in `exclude` (default `()`), and reject absolute paths and `..` components in their path inputs; `**` does not enter directory symlinks, and a missing root raises `FileNotFoundError`. `search` returns `{"path", "line", "text"}` records with 1-based line numbers and skips files containing a NUL byte; its regex is not a path input.
- Model-authored library changes are deferred: `lib_define`, `lib_remove`, scratch validation, `lib_auto_allow`, and `/lib-auto` are outside this implementation.

## Host functions

- `FYI()`: unchanged output.
- `help()`: the output limit, then `~/.harness/repl-notes.md` if present, then the agent's files, the supported Python subset and its alternatives, host functions, `run`'s contract with the `allow` names not in `deny` and the `deny` names, and public library functions (signature and docstring; files that failed to load this session are omitted).
- `run`, `lib_source`: above.

## Failures

- App denials and `NotImplementedError` from Monty are appended to `~/.harness/failures.jsonl`: `{"time", "session", "kind", "message", "code"}`, `kind` one of `denied`, `unsupported`. Existing `type check` records remain readable.
- Counts per `(kind, first line of message)` are loaded at startup and kept in memory.
- Each failure shows a note in `theme.warning` under its call: `<kind>: <first line> (seen N times)`, N including this one. Notes are never saved or sent.

## Terminal UI

- Status line: `<model> | N tokens | <permission>`, ` | tool reasoning` while on; activity as before without ` · classifying…`.
- Prompts show in the status line in `theme.approval`: `allow <operation>? (y/n, esc to stop)`.
- Enabled approval descriptions issue one independent, non-streaming chat-completions request with the substituted template as its sole user message and no tools or chat history. `${command}` is JSON describing only the pending operation: resolved executable, arguments, and working directory for command execution; the resolved directory for a directory approval.
- The original call stays visible. A wrapped, display-only block beneath it shows `Generating description…`, then `Description: <text>` or `Description unavailable: <error>`. Approval keys remain active throughout. Resolving or abandoning the approval removes the block and cancels pending generation; results cannot attach to another approval. Descriptions are not saved or sent to the chat model.
- Shift+Tab cycles `none → read-only → read-write`, saved to `[repl] permission`.

## Removed

`src/repl_driver.py`, the classifier (request, verdict notes, `[classifier] prompt`), and approval modes. The synthetic-call and separate `reasoning()` approval exemptions remain.
