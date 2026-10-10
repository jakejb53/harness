# RLM implementation plan

Spec: `SPEC-rlm.md`. Base contract: `SPEC.md`, which describes the pre-RLM harness and is updated only after implementation.

## 1. Spec and configuration

- `SPEC-rlm.md`: applied. Tools, mounts, and host functions fixed by depth; root tools `rlm` and `FYI` with a turn-start synthetic `FYI` call; `rlm` takes `task`; no ID awareness; store is a configured directory mounted at `/store`; library mounted at `/library`; direct tools take `root`; no truncation overflow; branches run sequentially with concurrency deferred; the display follows the deepest active agent.
- `config.toml.example` and `src/config.rs`: required `[rlm] max_depth` (integer ≥ 1), `[rlm] max_fan_out` (integer ≥ 1), `[rlm.store] path` (absolute path to an existing directory, opened at startup). Missing or invalid values are startup errors.
- Live `~/.harness/config.toml` needs the same keys before the binary starts; applied only with explicit approval.

## 2. Session tree

- `src/session.rs`: a new root session is `sessions/<name>/library/root.json`. Branch files are `sessions/<name>/library/<call-id>.json` at depth 1 and `sessions/<name>/deep/<call-id>.json` at depth 2 and deeper. `list` returns directory names under `sessions/`, oldest first by `root.json` mtime. `resume` loads `library/root.json`. `rename` renames the tree directory and refuses an existing destination.
- `LocalMetadata` gains required `repository: String`, `depth: u32`, and `parent: Option<String>` (absent at depth 0). Set at creation, never rewritten. `turn_token_cache` stays optional.
- Unanswered `rlm` calls found on resume receive `[not run: turn stopped]` like any call.

## 3. Agent extraction

- New `src/agent.rs`: `Agent` owns a `Session`, `Turn`, `Phase`, depth, tool set, REPL handle, and the latest `prompt_tokens`/`context_tokens` usage. `advance`, `start_request`, `on_stream`, `end_stream`, `save_reply`, `begin_call`, `run_call`, `on_repl`, `record_result`, `stop_turn`, and `next_turn_event` move from `src/app.rs` unchanged in behavior. Display-only notes and the failure log stay in the App; the agent reports failure facts and the App records them with `session` set to the tree directory name for the root and the call ID for a branch.
- `App` holds the agents as a stack: the root, then each active branch above its parent. Exactly one agent is active at a time, and the select loop awaits its events. The transcript, context overlay, token counter, and status line follow the top of the stack; the status line names the agent on screen. The active agent's pending prompt is the approval prompt; `y`/`n` answer it. `--system-prompt` applies to every agent.
- `src/client.rs`: `tool_definitions(depth)`: depth 0 is `[rlm, FYI]`; depths 1 through `max_depth - 1` are `[REPL, apply_patch, read_file, grep, list, rlm]`; `max_depth` omits `rlm`. `rlm` has one required string `task`. `FYI` has no parameters and returns the `Date`, `Prompt tokens`, and `Model` lines the REPL host function prints.
  - `rlm` at depth 1 through `max_depth - 1`, verbatim: `Delegate a task to a subagent.`
  - `rlm` at the root: the preamble `Delegate a task to a subagent and return its final answer. The subagent sees only your task text and works on the same repository with the following tools:` followed by one `name: description` entry per depth 1 tool, rendered from the depth 1 tool definitions so the two cannot differ. The depth 1 `rlm` entry therefore reads `rlm: Delegate a task to a subagent.`
  - No description mentions depth, the depth limit, or the tree.
  - `FYI`, verbatim: `Report the current date, the prompt-token count of the latest request, and the model name.`
- Root turn start: a synthetic `FYI` tool call with the synthetic ID kind opens each root turn, and the per-turn cleanup removes earlier `FYI` tool calls alongside `FYI()`/`help()` REPL calls. Branches keep the synthetic `FYI()` REPL call.
- `rlm` handling in `begin_call`: `rlm` calls run in the ordered queue like every other call. The first `max_fan_out` of a response's `rlm` calls run; the rest receive an error result naming the limit. An accepted call creates the branch session file with the task as its user message, queues its synthetic `FYI()`, pushes the branch agent onto the stack, and the parent waits in a `Phase::Delegating` until the branch ends.
- Branch completion: the final response content becomes the `rlm` result, recorded in the parent in call order; empty content records `(no output)`. The branch is popped and the parent's `advance` continues. A branch whose stream fails records the error in its own file and returns its partial final content followed by `[branch failed: <error>]` on its own line.
- Esc and exit stop every agent from the top of the stack down, each recording `[stopped by user]` and `[not run: turn stopped]` in its own file; a parent's `rlm` call awaiting a stopped branch receives `[not run: turn stopped]`.

## 4. REPL mounts and permissions

- `src/repl.rs`: `Pool` `max_processes = max_depth`, since a chain of nested branches from depth 1 to `max_depth` is what can hold checkouts at once. `Runtime` opens the store mount once at startup as `MountSpec` `/store` read-write. `Runtime::start(depth, library)` takes an optional library mount, opened by the App per tree at `/library` read-only from `sessions/<name>/library/`, supplied at depth 2 and deeper. Each feed passes the repository mount plus the mounts for its depth.
- `src/hostfs.rs`: `authorize` resolves by root. Under the repository: the current permission level, as today. Under `/store`: allowed. Under `/library`: reads allowed, writes denied. Anywhere else: denied. Rename authorizes each endpoint in its own root; Monty's mount table rejects cross-mount renames.
- `src/host.rs`: `help()` lists the mounts present for this depth with their modes. `FYI()` unchanged.

## 5. Direct tools

- `read_file`, `grep`, `list`, `apply_patch` arguments gain optional `root` with values `repository` (default), `store`, `library`. Each runtime holds a capability per root. Repository access follows the permission flags as today. Store access is always allowed. Library access is read-only; `apply_patch` with `root = "library"` returns an error. A root absent at this depth returns an error naming the depth.
- Tool descriptions document `root`. Historical argument objects display unchanged.

## 6. Validation and completion

- `cargo check`, `cargo test`, `cargo clippy`; existing tests updated for the new session layout.
- Smoke test by the user: root delegates, the display follows the branch and returns to the root, approval from a branch reaches the prompt, Esc stops the tree, branch files inspectable, store written by one branch read by another, depth 2 reads `/library`.
- After implementation: `README.md` and `SPEC.md` brought current.
