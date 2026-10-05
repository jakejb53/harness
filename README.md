# harness

A terminal coding agent for OpenAI-compatible chat completions endpoints. The model uses a Python REPL for computation and commands, plus direct `read_file`, `apply_patch`, `grep`, and `list` tools for files.

The app runs full screen and draws the complete saved conversation, word-wrapped to your terminal's width. The context editor controls which turns the model sees. Replies stream as they are generated, including the model's reasoning when the server provides it. When you exit, your terminal returns to what it showed before.

The REPL uses Monty, a sandboxed Python-subset interpreter. Repository filesystem access follows the permission level below. **Commands launched with `run()` are host processes running as you, outside the REPL sandbox.**

## Setup

Requires a Rust toolchain. The Monty interpreter is built into the app.

The app keeps its files in `~/.harness`: `config.toml`, `.env`, `sessions/`, `replib/`, optional `repl-notes.md`, and `failures.jsonl`. They are shared by every repository you run it in. `~/.harness` can be a symlink to a checkout of this repo.

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
tool_reasoning = false           # see Tool reasoning; /tool-reasoning switches it

[repl]
tool_description = """
Execute Python in a sandboxed Monty REPL. State lasts for one turn. Call help() for available functions and libraries. Use run() for host commands.
"""
permission = "none"              # none | read-only | read-write
max_memory_mb = 2048             # worker memory limit in MiB
output_limit = 100000            # bytes of output per call returned to the model

[commands]
deny = ["git", "rm"]             # always denied, even if also allowed
env_filter = ["GITHUB_TOKEN"]    # exact names removed from command environments

[commands.allow]
# cargo = "/absolute/path/to/cargo"  # configured commands run without asking
```

These are example values, not runtime defaults. All settings shown are required except `api_key_env`, `chat.prompt`, and individual allow-list entries. Keep `[commands.allow]` even when empty. Also copy the required `[theme]` table from `config.toml.example`: its foreground colors must be `#RRGGBB` strings. Colors load at startup without dimming or background changes. Missing required settings, unknown keys, and relative allow-list executable paths are startup errors. `tool_description` is sent verbatim to the model. See `config.toml.example` for a complete example.

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

Build with `cargo build`, then run `harness` (in `target/debug/`) from the directory the agent should work in, normally the root of a repository.

Each run starts a new conversation, saved to `~/.harness/sessions/YYYYMMDD-HHMMSS.json` when you send the first message or attach a file. The file name without `.json` is the session's name; `/rename <name>` changes it. To list saved sessions, oldest first, or continue one:

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

Type at the `> ` prompt and press Enter to send. The reply streams above the prompt: reasoning uses `theme.reasoning`, followed by the answer in `theme.assistant`.

Tool arguments appear as they stream in; execution waits for the complete response. Printed REPL output appears while it runs. Host commands return captured output when they finish. The model keeps calling tools until it answers without one. Esc stops the turn; a patch finishes accounting for any current file commit before reporting what changed.

The status line under the prompt shows the model, the number of tokens in use, and the filesystem permission level. It prefers the latest endpoint-reported total; before one is available, it sums the context overlay's per-turn totals. A `~` prefix marks a fallback containing any heuristic turn count. Pending approvals use `theme.approval`; press `y` to allow or `n` to deny.

While the model is working, you can keep typing your next message. It sends once the turn finishes and you press Enter.

**Situational awareness.** Each turn starts with the app calling `FYI()` in the REPL on the model's behalf. It prints the current date, prompt-token count, and model. The count prefers the latest endpoint-reported prompt usage; before one is available, it uses the context overlay's aggregate and prefixes `~` when any contributing turn is heuristic. The call and its output are shown and saved like any other. The model can call `FYI()` or `help()` itself at any time; `help()` lists the output limit and library functions and runs only on demand. Before adding `FYI()`, the app removes every earlier call made only of `FYI()`/`help()` from the conversation and its session file.

**Approval exemptions.** Except for the file-injection mechanism below, all synthetic tool calls, present and future, including `FYI()`, never require approval, whether the app generates them or the model calls them explicitly. Explicit `help()` calls also require no approval. This applies at every permission level. Unrelated code bundled with them, including code evaluated in arguments, follows normal permissions and approval rules. File injection authorizes only the app's attachment read; model-generated reads keep normal permissions.

### File injection

Put `@` in column one on its own attachment line, followed by the literal path:

```text
@src/app.rs
@docs/design notes.md
Review these files.
```

Everything after `@` is the path, including spaces; there is no trimming, quoting, glob, variable, or `~` expansion. Relative paths start at the launch directory; absolute paths and files outside the repository are allowed. A leading space makes the line ordinary text.

At the end of an attachment line, typing `@fi` lists matching files in the launch directory. Matches use case-sensitive prefixes and alphabetical order, including hidden files and symlinks to regular files. Up/Down selects, Tab completes only that line, and Esc dismisses the list. Enter loads the path as typed, not a different highlighted match. Other paths can be entered manually. Directory errors appear in the status line.

Enter loads attachments locally without starting a turn or requesting the model. The app reads each UTF-8 regular file in full, independently of REPL permissions and `output_limit`, and saves a synthetic `REPL` call with Python file-reading code and its result. The call ID identifies it as an injected file; the filename stays in the code. Neither the model nor Monty executes that call; `FYI()` and `help()` do not run. Empty files produce empty results. Every occurrence adds a separate instance; later turns and resumed sessions retain it without refreshing or replacing its contents.

Attachment command lines never enter the prompt or history. After successful loading, an attachment-only draft clears; mixed input keeps its ordinary text unsent, ready for a separate Enter. The next explicit message includes the files in model context. All files must load before changing the draft or history. Missing, unreadable, nonregular, or non-UTF-8 files block loading and preserve both. Loading holds draft editing; Esc cancels and preserves the original full draft and history.

A bordered context overlay in the transcript's upper right includes each injection as an assistant group and coupled tool-call position. See Context display below.

### Context display

The upper-right overlay is a compact context editor. User and assistant content, reasoning, and tool calls have separate rows; a call and its result share one row. App-generated synthetic calls are omitted. Each turn header shows the token count for its complete message slice, and each row shows its contribution within that turn. Turn labels use `theme.context_turn`; turn counts use `theme.context_tokens`; role labels use `theme.context_role`; row counts use `theme.context_message_tokens`; previews use `theme.status`. File-injection call/result pairs are standalone turns.

Click any row to hide or restore its completed turn in model requests. Hidden turns remain in the main transcript and context editor, but every part of their context-editor display is rendered at half brightness. The active turn cannot be hidden until it ends. Hidden state is saved with the session.

Counts are cached incrementally. Missing or changed turns are sent as progressive prefixes to the provider's `/tokenize` endpoint, derived by removing a final `/v1` from `[endpoint] base_url`. File-injection requests prepend an empty user message only for tokenization. If any prefix request fails, the complete turn uses `ceil(compact JSON UTF-8 bytes / 3.25)` estimates; `~` marks estimated counts. Unchanged cached turns make no tokenizer requests. Hidden turns keep their individual displayed counts but are excluded from aggregate context totals.

The expanded overlay is capped at nine-sixteenths of the transcript width and height. Its top border centers a bright `Context` header and the mouse wheel scrolls its rows. Click `[-]` to collapse the overlay to that one-row header; click `[+]` to reopen it at the retained width and scroll position. The overlay covers transcript cells without changing transcript wrapping or input width.

### Approval descriptions

Set `[approval_description] enabled = true` to request a concise explanation alongside each approval. Its `prompt` must contain `${command}`, replaced with the pending operation as JSON: the resolved executable, arguments, and working directory, or the directory being approved. Enclosing REPL code and chat history are not sent.

`[approval_description.endpoint]` has its own `base_url`, `model`, optional `api_key_env`, and optional `parameters` table. See `config.toml.example` for the complete configuration. An absent section or `enabled = false` disables descriptions; enabling requires both the prompt and endpoint.

While the request runs, `Generating description…` appears beneath the original call. You can press `y`, `n`, or Esc immediately. Resolving the approval cancels unfinished generation and removes the description. Request failures appear as `Description unavailable: …`; approval remains available. Descriptions are never saved in the session or sent to the chat model.

### Filesystem permissions

Shift+Tab cycles `none → read-only → read-write`; the choice is saved to `config.toml` and applies to the next filesystem operation, including during a call.

| Level | Repository filesystem access |
|---|---|
| `none` | Reads and writes are denied. |
| `read-only` | Reads are allowed; writes are denied. |
| `read-write` | Reads and writes are allowed. |

The repository is the directory from which you launch the app. Access outside it is denied at every level, including traversal and symlink escapes, without an approval prompt. Monty also rejects absolute symlink targets inside the repository; use relative targets. `Path.resolve()` normalizes paths lexically rather than following host symlinks.

Host command approvals are independent of these levels (see Host commands). `reasoning()` is not synthetic: a standalone `reasoning("...")` on a plain string has its own approval exemption at every level. It cannot exempt unrelated code.

### Direct file editing

`read_file(path, start_line?, end_line?)` returns numbered UTF-8 text. Bounds are 1-based and inclusive; omitted bounds mean line 1 and EOF. Reads use repository-relative paths, follow relative symlinks within the repository, and work in `read-only` or `read-write` mode. The existing `output_limit` applies; request smaller ranges when needed.

`apply_patch` accepts the familiar `*** Begin Patch` format with add, update, delete, and rename operations. Patch text is decoded once from JSON and handled directly by Rust, without Python or shell interpretation. Context must match exactly and uniquely; errors include hunk locations and relevant source context.

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
| Esc | Stop the model (with the command list open, close the list) |
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
| `/rename <name>` | Rename this session (its file in `~/.harness/sessions/`); refused if the name is taken |
| `/tool-reasoning` | Turn tool reasoning on or off (saved to `config.toml`) |
| `/tools` | Show every tool's name and full description in the conversation display; output is not saved or sent to the model |
| `/exit`, `/quit` | Exit |

Typing `/` opens a list of matching commands below the input: Up/Down to choose, Tab to complete, Enter to run (for `/rename`, to complete it so you can type the name), Esc to close.

To send a message that starts with `/`, begin it with a space.

## The REPL

REPL state lasts for one turn: from your message until the model's final answer. Each turn starts fresh. Static type checking is disabled; after an ordinary runtime exception, completed effects and surviving state remain available. A worker crash or memory-limit failure discards that state; the next call starts fresh and reloads the library, with the state loss reported explicitly.

Printed output is followed by the trailing expression's value, when not `None`, and any runtime error. Empty output becomes `(no output)`. Results are capped at `output_limit` bytes on a Unicode character boundary, with a notice stating how many bytes were omitted. The sandbox sees an empty environment.

`help()` lists the output limit, optional guidance from `~/.harness/repl-notes.md`, host functions, loaded public library functions, and allow-listed command names.

### Host commands

`run(argv, cwd=None)` executes a bare command name with literal arguments, without a shell. It returns `{"exit_code": int, "stdout": str, "stderr": str}` and prints nothing. Output is captured until the command finishes; a signal exit uses the negative signal number. Commands have no terminal or interactive stdin. Esc kills the command's process group.

Names in `[commands] deny` are blocked. Names in `[commands.allow]` run the configured absolute executable without asking. Other names are resolved through the app's `PATH` and require approval each time; missing commands produce an error. This is independent of the filesystem permission level.

Commands start in the repository root, independently of sandbox `os.chdir()`. An explicit `cwd` must be relative and contain no `..` components. If its symlinks resolve outside the repository, using that directory requires approval. Commands inherit the app's environment minus the exact names in `env_filter`.

### Library functions

Library functions are Monty-compatible Python functions you provide in `~/.harness/replib/*.py`. Files load in name order at the start of every turn. `help()` lists top-level functions whose names do not start with `_`, using their source signatures and docstrings. `lib_source(name)` returns the current contents of `replib/<name>.py`.

**Adding one.** Define a public function in a `.py` file in `replib/`:

```python
from pathlib import Path as _Path

def word_count(path: str) -> int:
    """Count the words in a file."""
    return len(_Path(path).read_text().split())
```

**Editing a description.** The docstring is exactly what the model reads, so it is the place to say what a function is for and when to use it. Edit it in place; the change takes effect from the next turn, since each turn starts a fresh REPL.

**Removing one.** Delete the function or its file. Top-level code executes during loading; private functions remain callable but are omitted from `help()`.

**Mistakes.** Load errors appear in the turn's first call (the app's `FYI()`); other files still load. Failed files are omitted from `help()`. Library code follows the same permissions as other REPL code.

This repo ships `replib/reasoning.py`, `replib/glob.py`, and `replib/search.py`. Use them through the `~/.harness` symlink or copy them into `~/.harness/replib/`:

- `reasoning(text)` does nothing and returns `None`. It gives the model a way to record its reasoning as part of the conversation, and the model is encouraged to call it: a call that is only `reasoning("...")` on a plain string never needs approval, and the app answers it with `(no output)` without running it.
- That answer, and tool reasoning (below), depend on it staying a no-op that prints nothing. Edit its docstring freely (it's what `help()` shows the model), but not its behavior.
- `glob(pattern, path=None, exclude=(".git", "target", "node_modules"))` returns sorted relative paths.
- `search(pattern, path=None, glob=None, exclude=(".git", "target", "node_modules"))` finds regex matches and returns `path`, 1-based `line`, and `text` records.

Both file helpers default to `.`, preserve the supplied relative root in results, skip directory symlinks, and reject absolute paths and `..` components in path inputs. Errors propagate rather than silently skipping unreadable files.

### Failure notes

App denials and unsupported Monty operations are appended to `~/.harness/failures.jsonl` with time, session, kind, message, and code. A note in `theme.warning` under the call shows the failure's first line and occurrence count across sessions. These notes are not sent to the model or saved in conversation files; the tool's error result is.

## Tool reasoning

Experimental. When on, the model sees its reasoning from earlier turns as `reasoning("...")` REPL calls, each followed by its `(no output)` result, instead of as native reasoning. The current turn keeps its native reasoning. Turn it on or off with `/tool-reasoning`; the status line shows `| tool reasoning` while it's on.

This conversion changes only what's sent and shown; saved reasoning stays unchanged, so switching it off restores the saved view at once, including for resumed conversations. It requires `reasoning()` in `replib/` (above).

## Conversation files

The saved `messages` array is authoritative session state. It interleaves provider messages with a local marker after each hidden turn; unmarked turns are model-visible. Requests omit markers and hidden turns, then apply the explicit tool-reasoning conversion described above. The optional top-level `_harness_local` object contains derived local metadata and is also never sent to providers. The system message is added separately and isn't saved; each request uses this invocation's `--system-prompt` override or the prompt loaded from `config.toml`, including for resumed conversations:

```json
{ "messages": [
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "", "reasoning": "...", "tool_calls": [ ... ] },
  { "role": "tool", "tool_call_id": "...", "content": "..." },
  { "role": "assistant", "content": "...", "reasoning": "..." },
  { "_harness_local": { "hidden": true } }
], "_harness_local": {
  "turn_token_cache": [ ... ]
} }
```

File injections are derived from encoded call IDs and filenames in saved Python code; there is no separate tracking field or sidecar. File contents live only in the matching tool result. New call IDs use one 32-character Base64url format; provider IDs are replaced before saving, and results use the saved IDs. Existing IDs and code are not migrated. Old IDs are not recognized as tracked attachments.

Session and `_harness_local` objects reject unknown fields. New local-only metadata must be explicitly added beneath `_harness_local`; obsolete fields receive no compatibility handling. A session containing an unknown or obsolete field fails to resume with a parse error.

Replies preserve other endpoint fields, such as reasoning under whatever name it uses (`reasoning`, `reasoning_content`, `reasoning_details`, …); outgoing reasoning changes only when tool reasoning is enabled. A reply you stop partway through is kept as far as it got. Tool results include their errors; display-only notices and failure-count notes are not saved in the conversation.
