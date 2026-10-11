# harness

A terminal coding agent for OpenAI-compatible chat completions endpoints. The agent you talk to delegates work to subagents with `rlm`. Subagents use a Python REPL for computation and commands, plus direct `read_file`, `apply_patch`, `grep`, and `list` tools for files, and can delegate further up to a configured depth.

The app runs full screen and draws the complete saved conversation, word-wrapped to your terminal's width. The context editor controls which turns the model sees. Replies stream as they are generated, including the model's reasoning when the server provides it. When you exit, your terminal returns to what it showed before.

The REPL uses Monty, a sandboxed Python-subset interpreter. Repository filesystem access follows the permission level below. **Commands launched with `run()` are host processes running as you, outside the REPL sandbox.**

## Setup

Requires a Rust toolchain. Install with `./install.sh`, which installs the `harness` binary into `~/.cargo/bin` and copies the shipped `replib/*.py` into `~/.harness/replib/`; it never touches `config.toml`, `sessions/`, or the store. The Monty interpreter is built into the app.

The app keeps its files in `~/.harness`: `config.toml`, `.env`, `sessions/`, `replib/`, optional `repl-notes.md`, and `failures.jsonl`. They are shared by every repository you run it in.

Create `~/.harness/config.toml`:

```toml
[endpoint]
base_url = "http://localhost:8000/v1"
model = "your-model-name"
api_key_env = "OPENAI_API_KEY"   # optional; omit for servers that need no key

[chat]
prompt = """
...optional system message sent at the start of every request...
"""

[repl]
tool_description = """
Execute Python in a sandboxed Monty REPL. State lasts for one turn. Call help() for available functions and libraries. Use run() for host commands.
"""
permission = "none"              # none | read-only | read-write
max_memory_mb = 2048             # worker memory limit in MiB
output_limit = 100000            # bytes of output per call returned to the model

[rlm]
max_depth = 2                    # deepest subagent depth, at least 1 (see Agents)
# subagent_prompt = "..."        # optional; subagents' system message (see Agents)

[rlm.store]
path = "/absolute/path/to/store" # existing directory shared by subagents (see Agents)

[commands]
deny = ["git", "rm"]             # always denied, even if also allowed
env_filter = ["GITHUB_TOKEN"]    # exact names removed from command environments

[commands.allow]
# cargo = "/absolute/path/to/cargo"  # configured commands run without asking
```

These are example values, not runtime defaults. All settings shown are required except `api_key_env`, `chat.prompt`, and individual allow-list entries. Keep `[chat]` and `[commands.allow]` even when empty. Also copy the required `[theme]` table from `config.toml.example`: its foreground colors must be `#RRGGBB` strings. Colors load at startup without dimming or background changes. Missing required settings, unknown keys, and relative allow-list executable paths are startup errors. `tool_description` is sent verbatim to the model. `[rlm.store] path` must be an absolute path (`~` is not expanded) to an existing directory; the app never creates it. The store must lie outside the launch directory and not contain it, or every subagent REPL call fails; neither the store nor the launch directory may contain `~/.harness/sessions/`, or REPL calls of subagents at depth 2 and deeper fail. Up to `max_depth` REPL workers run at once, each limited by `max_memory_mb`. See `config.toml.example` for a complete example.

Optional `[endpoint.parameters]` entries are sent as top-level request fields. Uncomment the table heading and desired values in `config.toml` to enable overrides:

```toml
# [endpoint.parameters]
# temperature = 0.7
# top_p = 0.9
# top_k = 40
# presence_penalty = 0.5
# repetition_penalty = 1.1
# max_tokens = 8192
```

Omitted parameters use endpoint defaults. The endpoint validates parameter names and ranges. Harness rejects reserved keys (`model`, `messages`, `tools`, `stream`, `stream_options`, `n`), non-finite numbers, and unquoted TOML dates/times. Strings, numbers, booleans, arrays, and nested tables are supported.

If the endpoint needs a key, put it in `~/.harness/.env` (and keep that file out of version control):

```
OPENAI_API_KEY=sk-...
```

A variable already set in your shell takes precedence over `.env`. The app reads `.env` privately; it does not export those values to command processes.

## Running

Run `harness` from the directory the agent should work in, normally the root of a repository.

Each run starts a new conversation, saved under `~/.harness/sessions/YYYYMMDD-HHMMSS/` when you send the first message. The directory name is the session's name; `/rename <name>` changes it. To list saved sessions, least recently updated first, or continue one:

```
harness --resume
harness --resume 20260925-143012
```

The earlier conversation is shown, and new messages are added to the same file.

Override `[chat] prompt` for this invocation with literal text:

```sh
harness --system-prompt "Your system prompt"
```

This also works with `--resume <NAME>`. The text is used verbatim, including an empty string, without changing configuration or saving the prompt in the session. Omit the option to use the configured prompt.

## Using it

Type at the `> ` prompt and press Enter to send. The reply streams above the prompt: reasoning uses `theme.reasoning`, followed by the answer in `theme.assistant`. The on-screen agent's system message, when it has one, heads the transcript in `theme.system`.

Tool arguments appear as they stream in; execution waits for the complete response. Printed REPL output appears while it runs. Host commands return captured output when they finish. The model keeps calling tools until it answers without one. Esc stops the turn and every running subagent; a patch finishes accounting for any current file commit before reporting what changed.

The status line under the prompt shows the name of the agent on screen, the model, the number of tokens in use, and the filesystem permission level. It prefers the latest endpoint-reported total; before one is available, it sums the context overlay's per-turn totals. A `~` prefix marks a fallback containing any heuristic turn count. Pending approvals use `theme.approval`; press `y` to allow or `n` to deny.

While the model is working, you can keep typing your next message. It sends once the turn finishes and you press Enter.

**Situational awareness.** Each root turn starts with the app calling `FYI` on the model's behalf; each subagent starts with the app calling `FYI()`, `help()`, `store_summary()`, and, where `/library` is mounted, `library_summary()` in its REPL. `FYI` prints the current date, the agent's prompt-token count, and the model. The count prefers the agent's latest endpoint-reported prompt usage; before one is available, it uses the context overlay's aggregate and prefixes `~` when any contributing turn is heuristic. The calls and their output are shown and saved like any other. Agents can call `FYI` themselves at any time, and subagents `help()`, which describes the subagent's REPL (see The REPL). `store_summary()` and `library_summary()` print, for `/store` and `/library`, the regular-file count, total bytes, latest modification time, and up to five most recently modified paths, or `<mount>: empty` when it has none; subagents can call each wherever its mount exists. Before adding `FYI` to a root turn, the app removes every earlier `FYI` call from the conversation and its session file.

**Approval exemptions.** All synthetic tool calls, present and future (the `FYI` tool, `FYI()`, `help()`, `store_summary()`, and `library_summary()`), never require approval, whether the app generates them or the model calls them explicitly. This applies at every permission level. Unrelated code bundled with them, including code evaluated in arguments, follows normal permissions and approval rules.

### Agents

The agent you talk to, the root, only delegates. Its tools are `rlm` and `FYI`; it has no REPL and no file access. `rlm` takes a `task`, which becomes a subagent's first message verbatim, and a short `name` shown to you. The subagent works until it answers without a tool call; that answer, capped at `output_limit`, is the `rlm` result. A subagent runs once and is never resumed.

Subagents have the REPL, `read_file`, `apply_patch`, `grep`, and `list`, plus `rlm` while their depth is below `[rlm] max_depth`. The root is depth 0 and each subagent is one deeper than its caller, so `max_depth = 1` lets only the root delegate. The `rlm` calls in one response run one at a time, in order. Every agent receives the same system message, `[chat] prompt` or `--system-prompt` when given, unless the optional `[rlm] subagent_prompt` is set; subagents then receive it instead, verbatim.

While a subagent runs, the transcript, context overlay, and status line show it, and return to its caller when it ends. The status line and the overlay header name the agent on screen: the session name for the root, the `rlm` name for a subagent. Approvals from any subagent appear at the one approval prompt. Esc stops every agent: the deepest agent's partial response, followed by `[stopped by user]`, becomes the `rlm` result at every level. A subagent that fails returns its partial response followed by `[branch failed: <error>]`.

**Store.** `[rlm.store] path` is one directory shared by every subagent, session, and repository; its contents persist. Subagents reach it at `/store` in the REPL, read-write at every permission level; the root has no access. Agents pass data by writing it there and naming the path in a task or answer. The app holds no locks.

**Library.** Agents at depth 2 and deeper can read this session's root and depth 1 conversation files, read-only, at `/library`.

`read_file`, `apply_patch`, `grep`, and `list` take an optional `root`: `repository` (default), `store`, or `library`, each offered only where the agent has it; `apply_patch` never offers `library`. Paths are then relative to that directory; in the tool sections below, "the repository" means the chosen root, and permission requirements apply only to `repository`. The transcript and context overlay show an `apply_patch` call's `root`, when given, as a `root: <value>` line above its patch.

### Context display

The upper-right overlay is a compact context editor. User and assistant content, reasoning, and tool calls have separate rows; a call and its result share one row. App-generated synthetic calls are omitted. Each turn header shows the token count for its complete message slice, and each row shows its contribution within that turn. Turn labels use `theme.context_turn`; turn counts use `theme.context_tokens`; role labels use `theme.context_role`; row counts use `theme.context_message_tokens`; previews use `theme.status`.

Click any row to hide or restore its completed turn in model requests. Hidden turns remain in the main transcript and context editor, but every part of their context-editor display is rendered at half brightness. The active turn cannot be hidden until it ends. Hidden state is saved with the session.

Counts are cached incrementally. Missing or changed completed turns are sent as progressive prefixes to the provider's `/tokenize` endpoint, derived by removing a final `/v1` from `[endpoint] base_url`. If any prefix request fails, the complete turn uses `ceil(compact JSON UTF-8 bytes / 3.25)` estimates, as the active turn does until it ends; `~` marks estimated counts. Unchanged cached turns make no tokenizer requests. Hidden turns keep their individual displayed counts but are excluded from aggregate context totals.

The expanded overlay is capped at nine-sixteenths of the transcript width and height. Its top border centers a `[Context][<root>]…[<name>]` header in `theme.assistant`, naming each agent from the root to the one on screen as the status line does; when space runs short, the oldest names collapse into one `[…]`, then the on-screen name shortens with an ellipsis. The mouse wheel scrolls its rows. Click `[-]` to collapse the overlay to that one-row header; click `[+]` to reopen it at the retained width and scroll position. The overlay covers transcript cells without changing transcript wrapping or input width.

### Approval descriptions

Set `[approval_description] enabled = true` to request a concise explanation alongside each approval. Its `prompt` must contain `${command}`, replaced with the pending operation as JSON: the resolved executable, arguments, and working directory, or the directory being approved. Enclosing REPL code and chat history are not sent.

`[approval_description.endpoint]` has its own `base_url`, `model`, optional `api_key_env`, and optional `parameters` table. See `config.toml.example` for the complete configuration. An absent section or `enabled = false` disables descriptions; enabling requires both the prompt and endpoint.

While the request runs, `Generating description…` appears beneath the original call. You can press `y`, `n`, or Esc immediately. Resolving the approval cancels unfinished generation and removes the description. Request failures appear as `Description unavailable: …`; approval remains available. Descriptions are never saved in the session or sent to the chat model.

### Filesystem permissions

Shift+Tab cycles `none → read-only → read-write`; the choice is saved to `config.toml` and applies to every agent's next filesystem operation, including during a call.

| Level | Repository filesystem access |
|---|---|
| `none` | Reads and writes are denied. |
| `read-only` | Reads are allowed; writes are denied. |
| `read-write` | Reads and writes are allowed. |

The repository is the directory from which you launch the app; the REPL mounts it at the virtual path `/repository`, which is also its working directory, so REPL paths and errors never show its host path, and a host path written in REPL code is outside every mount. Access outside it is denied at every level, including traversal and symlink escapes, without an approval prompt, except a subagent's `/store` and `/library` (see Agents). Monty also rejects absolute symlink targets inside the repository; use relative targets. `Path.resolve()` normalizes paths lexically rather than following host symlinks.

Host command approvals are independent of these levels (see Host commands). `reasoning()` is not synthetic: a standalone `reasoning("...")` on a plain string has its own approval exemption at every level. It cannot exempt unrelated code.

### Direct file editing

`read_file(path, start_line?, end_line?)` returns numbered UTF-8 text. Bounds are 1-based and inclusive; omitted bounds mean line 1 and EOF. Reads use repository-relative paths, follow relative symlinks within the repository, and work in `read-only` or `read-write` mode. The existing `output_limit` applies; request smaller ranges when needed.

`apply_patch` accepts the familiar `*** Begin Patch` format with add, update, delete, and rename operations, plus `*** Replace File: path`, which replaces the whole text of an existing file from `+` lines while keeping its line endings, final-newline state, and permissions. Patch text is decoded once from JSON and handled directly by Rust, without Python or shell interpretation. Context must match exactly and uniquely; errors include hunk locations and relevant source context. When anchor text repeats, `@@ line 63: ## conventions` targets the occurrence at line 63 of the file before the update and fails if that line holds other text.

Editing requires `read-write` permission. All paths must be repository-relative, even for files inside the repository. Text updates follow relative symlinks within the repository; rename and deletion act on the named entries. Following absolute symlink targets or escaping the repository is rejected. Add/rename destinations are checked for absence during validation; ordinary filesystem errors are reported.

The complete patch is parsed first, then each operation is validated and applied in order against the current filesystem. A later failure stops the patch and leaves earlier changes applied. Text replacements preserve existing line endings, final-newline state, and executable permissions; they replace file identity rather than updating other hard links, and do not copy other metadata. Rename-only operations use filesystem rename without reading contents. Results report actual changes before numbered file views; the entire result is subject to `output_limit`.

### Searching files

The `grep` tool accepts structured search parameters:

```json
{"pattern":"this|that","path":"src","include":"*.rs","context":2}
```

`pattern` is required and uses extended regex syntax. Defaults are `path="."` (the repository root), `fixed=false`, `ignore_case=false`, `context=0`, and `include=""`. `fixed` selects literal strings; `ignore_case` controls content matching. `include` is a case-sensitive file-basename glob applied at every depth, with empty meaning no filter. Paths are literal and repository-relative; directories are always searched recursively.

Matching and context lines always use `file:line:text`, with `--` between context groups. Binary files are skipped. Searches require `read-only` or `read-write` permission and stay inside the repository. The existing `output_limit` applies, and Esc cancels the search.

Searches cover regular files; named pipes, sockets, and devices are skipped with brief warnings. Recursive searches include hidden and ignored files and skip symlinks with warnings. Every warning names the skipped repository-relative path in the tool result.

### Listing files

`list` defaults to the repository root, top-level entries only: `path="."`, `recursive=false`, `include=""`. To discover Rust files beneath a directory:

```json
{"path":"src","recursive":true,"include":"*.rs"}
```

Output is sorted by repository-relative path, one entry per line: `path  size  mtime`. Paths are quoted and escaped; directories end in `/`, and other nonregular entries use `[symlink]` or `[special]`. Sizes are exact metadata bytes, including a directory's own size rather than its contents. Modification times are UTC with nine fractional-second digits.

Hidden entries are included. The case-sensitive basename filter controls output, not recursion. Symlinks use their own metadata and are never followed during recursive descent. Listing requires `read-only` or `read-write` permission, stays inside the repository, and uses the existing output limit and Esc cancellation.

### Keys

| Key | Action |
|---|---|
| Enter | Send |
| Ctrl+J | New line |
| Ctrl+K | Delete to end of line |
| Ctrl+U | Delete to start of line |
| Esc | Stop the model and every running subagent (with the command list open, close the list) |
| Shift+Tab | Change filesystem permission |
| y / n | Allow or deny the pending host operation |
| PageUp / PageDown | Scroll the conversation a screen |
| Mouse wheel | Scroll the conversation a line |
| Mouse drag | Select text; it's copied when you release |

Pasting multi-line text never sends it; press Enter when ready.

When you scroll up, the view stays put while the model writes; scroll back to the bottom to follow it again. Copying uses the OSC 52 escape sequence, which some terminals don't support or need enabled (in iTerm2, allow clipboard access in settings; in tmux, set `set-clipboard on`). Where it doesn't work, your terminal's own selection still does, usually by holding Option (macOS) or Shift while dragging.

### Commands

| Command | Action |
|---|---|
| `/rename <name>` | Rename this session (its directory in `~/.harness/sessions/`); refused if the name is taken, empty, contains `/`, or starts with `.`, or while a subagent is running |
| `/tools` | Show every tool's name and full description for the agent on screen in the conversation display; output is not saved or sent to the model |
| `/new`, `/clear` | Start a new, empty session; the current one stays saved for `--resume`. Refused while a turn is running |
| `/exit`, `/quit` | Exit |

Typing `/` opens a list of matching commands below the input: Up/Down to choose, Tab to complete, Enter to run (for `/rename`, to complete it so you can type the name), Esc to close.

To send a message that starts with `/`, begin it with a space.

## The REPL

Each subagent has its own REPL, which starts fresh and lasts until the subagent's final answer; no other agent sees its state. Static type checking is disabled; after an ordinary runtime exception, completed effects and surviving state remain available. A worker crash or memory-limit failure discards that state; the next call starts fresh and reloads the helper functions, with the state loss reported explicitly.

Printed output is followed by the trailing expression's value, when not `None`, and any runtime error. Empty output becomes `(no output)`. Results are capped at `output_limit` bytes on a Unicode character boundary, with a notice stating how many bytes were omitted. The sandbox sees an empty environment.

`help()` prints the output limit, the contents of `~/.harness/repl-notes.md` when present, and five sections: Files (the repository with its current permission level, plus `/store` and `/library` where mounted, and how REPL code and the direct tools reach each), Python (the supported subset and what to use instead), REPL functions (`FYI`, `help`, `run`, `lib_source`, and `store_summary` and `library_summary` where their mounts exist), Commands (how to call `run()`, with the allow-listed and denied names), and Helper functions (loaded public helpers). The Python section's `Use instead` lines come from one table of unsupported names that agents commonly try, each with its supported alternative, and a REPL call whose result ends with an exception listed in that table gets one more line: `<name> is not available in this REPL; use <alternative>.` A tool call naming one of those REPL functions or loaded helper functions gets `Unknown tool: <name>` followed by a pointer to call it inside REPL code, with its signature.

### Host commands

`run(argv, cwd=None)` runs `argv[0]` with the remaining items as literal arguments, without a shell. It returns `{"exit_code": int, "stdout": str, "stderr": str}` and prints nothing. Output is captured until the command finishes; a signal exit uses the negative signal number. Commands have no terminal, stdin, or timeout. Esc kills the command's process group.

`argv[0]` is a command name or a relative path containing `/`. Names in `[commands] deny` are blocked. Names in `[commands.allow]` run the configured absolute executable without asking. Other names resolve to an executable file on the app's `PATH` and require approval each time; a name not found raises `FileNotFoundError`. A path names an executable inside the repository, resolved against `cwd`; it requires approval each time, and the deny and allow lists do not apply to it. Absolute paths, `..` components, and paths resolving outside the repository are denied, and a path that is not an executable file raises `PermissionError`. Denials and refused approvals raise `PermissionError`. None of this depends on the filesystem permission level.

Commands start in the repository root, independently of sandbox `os.chdir()`. An explicit `cwd` must be an existing directory, relative, with no `..` components. If its symlinks resolve outside the repository, using that directory requires approval. Commands inherit the app's environment minus the exact names in `env_filter`. They run on the host, where `/repository`, `/store`, and `/library` do not exist.

### Helper functions

Helper functions are Monty-compatible Python functions you provide in `~/.harness/replib/*.py`. Files load in name order at the start of every subagent's REPL. `help()` lists top-level functions whose names do not start with `_`, using their source signatures and docstrings. `lib_source(name)` returns the current contents of `replib/<name>.py`.

**Adding one.** Define a public function in a `.py` file in `replib/`:

```python
from pathlib import Path as _Path

def word_count(path: str) -> int:
    """Count the words in a file."""
    return len(_Path(path).read_text().split())
```

**Editing a description.** The docstring is exactly what the model reads, so it is the place to say what a function is for and when to use it. Edit it in place; the change takes effect from the next subagent, since each subagent starts a fresh REPL.

**Removing one.** Delete the function or its file. Top-level code executes during loading; private functions remain callable but are omitted from `help()`.

**Mistakes.** Load errors appear in the subagent's first call (the app's `FYI()`); other files still load. Failed files are omitted from `help()`. Helper code follows the same permissions as other REPL code.

This repo ships `replib/reasoning.py`, `replib/glob.py`, and `replib/search.py`. `install.sh` copies them into `~/.harness/replib/` and overwrites them on every install; your own files there are unaffected.

- `reasoning(text)` does nothing and returns `None`. It gives the model a way to record its reasoning as part of the conversation, and the model is encouraged to call it: a call that is only `reasoning("...")` on a plain string never needs approval, and the app answers it with `(no output)` without running it. That answer depends on it staying a no-op that prints nothing.
- `glob(pattern, path=None, exclude=())` returns sorted relative paths.
- `search(pattern, path=None, glob=None, exclude=())` finds regex matches in the files that the `glob()` pattern `glob` (default `**/*`) selects and returns `path`, 1-based `line`, and `text` records. Files containing a NUL byte are skipped as binary, as the `grep` tool skips them.

Both file helpers default to `.`, preserve the supplied relative root in results, skip only the entry names you pass in `exclude`, and reject absolute paths and `..` components in path inputs, so they cannot reach `/store` or `/library`; `**` does not enter directory symlinks. Errors propagate rather than silently skipping unreadable files; a missing root raises `FileNotFoundError`.

### Failure notes

App denials and unsupported Monty operations are appended to `~/.harness/failures.jsonl` with time, session (the session name for the root, the call ID for a subagent), kind, message, and code. A note in `theme.warning` under the call shows the failure's first line and occurrence count across sessions. These notes are not sent to the model or saved in conversation files; the tool's error result is.

## Conversation files

Each session is a directory, `~/.harness/sessions/<name>/`. `library/root.json` is your conversation with the root. Each subagent's conversation is a file named by the ID of the `rlm` call that started it: `library/<id>.json` at depth 1, `deep/<id>.json` deeper. `--resume` lists and loads only directories containing `library/root.json`. Every file uses the format below.

The saved `messages` array is authoritative session state. It interleaves provider messages with a local marker after each hidden turn; unmarked turns are model-visible. Requests omit markers and hidden turns. The required top-level `_harness_local` object is never sent to providers. It records, at creation, `repository` (the canonical launch directory), `depth` (0 for the root), and, for a subagent, `parent` (`root` at depth 1, otherwise the parent's call ID); the root's file also holds the derived `turn_token_cache`. The system message is added separately and isn't saved; each request uses the agent's system message as this invocation resolves it (see Agents), including for resumed conversations:

```json
{ "messages": [
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "", "reasoning": "...", "tool_calls": [ ... ] },
  { "role": "tool", "tool_call_id": "...", "content": "..." },
  { "role": "assistant", "content": "...", "reasoning": "..." },
  { "_harness_local": { "hidden": true } }
], "_harness_local": {
  "repository": "/path/to/repo",
  "depth": 0,
  "turn_token_cache": [ ... ]
} }
```

New call IDs use one 32-character Base64url format; provider IDs are replaced before saving, and results use the saved IDs.

Session and `_harness_local` objects reject unknown fields. New local-only metadata must be explicitly added beneath `_harness_local`; obsolete fields receive no compatibility handling. A session containing an unknown or obsolete field fails to resume with a parse error.

Replies preserve other endpoint fields, such as reasoning under whatever name it uses (`reasoning`, `reasoning_content`, `reasoning_details`, …), which later requests send back unchanged. A reply you stop partway through is kept as far as it got. Tool results include their errors; display-only notices and failure-count notes are not saved in the conversation.
