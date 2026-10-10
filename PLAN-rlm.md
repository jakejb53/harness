# RLM implementation plan

Spec: `SPEC-rlm.md`. Base contract: `SPEC.md`, which describes the pre-RLM harness and is updated only after implementation.

## 1. Spec and configuration

- `SPEC-rlm.md`: applied. Tools, mounts, and host functions fixed by depth; root tools `rlm` and `FYI` with a turn-start synthetic `FYI` call; `rlm` takes `task`; agents are told no IDs and no depth; store is a configured directory mounted at `/store`; library mounted at `/library`; direct tools take `root`; truncated results are not written to the store; branches run sequentially with concurrency deferred; the display follows the deepest active agent.
- `config.toml.example` and `src/config.rs`: required `[rlm] max_depth` (integer ≥ 1), `[rlm] max_fan_out` (integer ≥ 1), `[rlm.store] path` (absolute path to an existing directory, opened at startup). Missing or invalid values are startup errors. `[chat] tool_reasoning` is removed.
- No startup check that the harness directory lies outside the repository.
- Live `~/.harness/config.toml` needs the same changes before the binary starts; applied only with explicit approval.

## 2. Session tree

- `src/session.rs`: a new root session is `sessions/<name>/library/root.json`. Branch files are `sessions/<name>/library/<call-id>.json` at depth 1 and `sessions/<name>/deep/<call-id>.json` at depth 2 and deeper. `list` returns directory names under `sessions/`, oldest first by `root.json` mtime. `resume` loads `library/root.json`. `rename` renames the tree directory and refuses an existing destination.
- `LocalMetadata` gains required `repository: String`, `depth: u32`, and `parent: Option<String>` (absent at depth 0). Set at creation, never rewritten. `turn_token_cache` stays optional. The root records `depth` 0. `repository` is the canonical launch directory of the process that creates the session; resume does not compare it. `parent` is `root` for a depth 1 branch and the parent's call ID deeper.
- Unanswered `rlm` calls found on resume receive `[not run: turn stopped]` like any call.

## 3. Agent extraction

- New `src/agent.rs`: `Agent` owns a `Session`, `Turn`, `Phase`, depth, tool set, REPL handle, and the latest `prompt_tokens`/`context_tokens` usage. `advance`, `start_request`, `on_stream`, `end_stream`, `save_reply`, `begin_call`, `run_call`, `on_repl`, `record_result`, `stop_turn`, and `next_turn_event` move from `src/app.rs` unchanged in behavior. Display-only notes and the failure log stay in the App; the agent reports failure facts and the App records them with `session` set to the tree directory name for the root and the call ID for a branch. Agent methods receive the App's shared resources (settings, HTTP client, REPL runtime, direct-tool runtimes) per call and return outcomes the App acts on (start a branch, ended with content, failure facts, notes). The App never changes an agent's `Phase` directly.
- `App` holds the agents as a stack: the root, then each active branch above its parent. Exactly one agent is active at a time, and the select loop awaits its events. The transcript, context overlay, token counter, and status line follow the top of the stack; the status line names the agent on screen by its session identity, the tree directory name for the root and the call ID for a branch. `/tools` shows the tools of the agent on screen. Only completed turns are tokenized and a branch's one turn is active whenever it is on screen, so branch files get no token cache. Each agent's `FYI` reports its own latest usage by the existing `FYI()` rule. The active agent's pending prompt is the approval prompt; `y`/`n` answer it. `--system-prompt` applies to every agent.
- `src/client.rs`: `tool_definitions(depth)`: depth 0 is `[rlm, FYI]`; depths 1 through `max_depth - 1` are `[REPL, apply_patch, read_file, grep, list, rlm]`; `max_depth` omits `rlm`. `rlm` has one required string `task`. `FYI` has no parameters and returns the `Date`, `Prompt tokens`, and `Model` lines the REPL host function prints. The `task` property's description is `The task for the subagent.` A call to a tool outside the agent's `tool_definitions(depth)` returns `Unknown tool: <name>`.
  - `rlm` at depth 1 through `max_depth - 1`, verbatim: `Delegate a task to a subagent.`
  - `rlm` at the root: the preamble `Delegate a task to a subagent and return its final answer. The subagent sees only your task text and works on the same repository with the following tools:` followed by one `name: description` entry per depth 1 tool, rendered from the depth 1 tool definitions so the two cannot differ. Entries are separated by blank lines. The depth 1 `rlm` entry, present when `max_depth` ≥ 2, therefore reads `rlm: Delegate a task to a subagent.`
  - No description mentions depth, the depth limit, or the tree.
  - `FYI`, verbatim: `Report the current date, the prompt-token count of the latest request, and the model name.`
- Root turn start: a synthetic `FYI` tool call with the synthetic ID kind opens each root turn, and the per-turn cleanup removes earlier `FYI` tool calls alongside `FYI()`/`help()` REPL calls. Branches keep the synthetic `FYI()` REPL call.
- `rlm` handling in `begin_call`: `rlm` calls run in the ordered queue like every other call. The first `max_fan_out` of a response's `rlm` calls run; the rest receive an error result naming the limit. An accepted call puts the parent in `Phase::Delegating` and returns a start-branch outcome; the App creates the branch session file with the task as its user message, queues its synthetic `FYI()`, and pushes the branch agent onto the stack. The parent waits until the branch ends.
- Branch completion: the final response content becomes the `rlm` result, recorded in the parent in call order; empty content records `(no output)`. The branch is popped and the parent's `advance` continues. A branch that fails, by stream error or any other error, ends its turn the way a stream error ends one today: its file keeps the partial reply, calls that never ran get `[not run: turn stopped]`, and the error is not stored in it. The branch returns the text streamed so far in its failed response followed by `[branch failed: <error>]` on its own line.
- Esc and exit stop every agent from the top of the stack down. The deepest agent records its stop as any stopped turn does today. In every agent, calls that never ran get `[not run: turn stopped]`. The deepest agent's partial response followed by `[stopped by user]` on its own line becomes its parent's `rlm` result and passes up unchanged as each ancestor's `rlm` result to the root.
- `/rename` while a branch runs shows `couldn't rename: a subagent is running` and keeps the input, like other rename refusals.

## 4. REPL mounts and permissions

- `src/repl.rs`: `Pool` `max_processes = max_depth`, since a chain of nested branches from depth 1 to `max_depth` is what can hold checkouts at once. `Runtime` opens the store mount once at startup as `MountSpec` `/store` read-write. `Runtime::start(depth, library)` takes an optional library mount, opened by the App per tree at `/library` read-only from `sessions/<name>/library/`, supplied at depth 2 and deeper. Each feed passes the repository mount plus the mounts for its depth.
- `src/hostfs.rs`: `authorize` resolves by root. Under the repository: the current permission level, as today. Under `/store`: allowed. Under `/library`: reads allowed, writes denied. Anywhere else: denied. Rename authorizes each endpoint in its own root; Monty's mount table rejects cross-mount renames.
- `src/host.rs`: `help()` lists the mounts present for this depth with their modes and a short instruction for each: `/library` holds session transcripts available to inspect; `/store` is a shared knowledge base and how to use it. `help()` is how an agent learns what it has; agents are told no IDs and no depth, and the text mentions neither depth nor the tree. The exact text is shown to the user for approval before it is implemented. `FYI()` unchanged.

## 5. Direct tools

- `read_file`, `grep`, `list`, `apply_patch` arguments gain optional `root` with values `repository` (default), `store`, `library`. Each runtime holds a capability per root. Repository access follows the permission flags as today. Store access is always allowed. Library access is read-only; `apply_patch` with `root = "library"` returns an error. Each depth's definitions offer only its roots; `library` appears from depth 2. A call naming an absent root returns an error naming that root.
- Tool descriptions document `root`. Historical argument objects display unchanged.

## 6. Removals

- Tool reasoning: the conversion, `[chat] tool_reasoning`, `/tool-reasoning`, the status indicator, and deterministic reasoning IDs. `replib/reasoning.py` and the `reasoning()` approval exemption remain.
- File injection: `src/injection.rs`, attachment lines, `@` completion, the injected-file call-ID kind, and its cleanup, overlay, and tokenizer exceptions.

## 7. Validation and completion

- `cargo check`, `cargo test`, `cargo clippy`; existing tests updated for the new session layout.
- Smoke test by the user: root delegates, the display follows the branch and returns to the root, approval from a branch reaches the prompt, Esc stops the tree, branch files inspectable, store written by one branch read by another, depth 2 reads `/library`.
- After implementation: `README.md` and `SPEC.md` brought current. `SPEC.md` lines 50 and 74 already misstate the synthetic `FYI()` call's ID kind.

## Known issues outside this plan

- `src/session.rs:209`: an entry with both `role` and `_harness_local` decodes as a provider message, and `_harness_local` is sent to the provider through `Message.extra`.
- `src/session.rs:612-616`: `rename` checks that the destination is absent, then renames in a separate step.
- `src/session.rs:390`: session names have one-second resolution; two launches in the same second share a name.
