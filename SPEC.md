# harness

Terminal chat client for an OpenAI-compatible chat completions endpoint. A root agent delegates with `rlm` to branch agents, which have a sandboxed Monty Python-subset REPL and direct Rust `read_file`, `apply_patch`, `grep`, and `list` tools. Agents, the session tree, the library, and the store: `SPEC-rlm.md`.

## Files

All paths are relative to the harness directory, `~/.harness` (home directory from the OS; not found is an error before the TUI starts), shared by every working directory. The launch working directory is the repository: the REPL's `/repository` mount, the direct tools' default root, and the default host-command working directory.

- `config.toml`:
  ```toml
  [endpoint]
  base_url = "http://host:8000/v1"   # requests go to {base_url}/chat/completions; trailing `/` trimmed
  model = "your-model-name"
  api_key_env = "OPENAI_API_KEY"     # optional: names the variable holding the key; omitted = no Authorization header

  [chat]
  prompt = """..."""                 # system message for every agent; omitted = none

  [repl]
  tool_description = "..."           # sent verbatim in the REPL tool definition
  permission = "none"                # "none" | "read-only" | "read-write"; rewritten by Shift+Tab
  max_memory_mb = 2048               # worker memory limit in MiB
  output_limit = 100000              # bytes of output per call sent to the model

  [rlm]
  max_depth = 2                      # deepest branch depth; integer >= 1
  subagent_prompt = """..."""        # optional: system message for branches instead of [chat] prompt

  [rlm.store]
  path = "/absolute/path/to/store"   # existing directory; never created

  [commands]
  deny = ["git", "rm"]               # command names; deny takes precedence over allow
  env_filter = ["GITHUB_TOKEN"]      # exact names removed from command environments

  [commands.allow]                   # command name = absolute executable path
  ```
  All settings shown are required except `api_key_env`, `chat.prompt`, and `rlm.subagent_prompt`; the values above are examples, not runtime defaults. The required `[theme]` table is specified under Terminal UI. Keep `[chat]` and `[commands.allow]` even when empty; allow values must be absolute paths. `max_depth` must be at least 1. `[rlm.store] path` must be an absolute path to an existing directory, symlinks followed. Unknown keys are rejected. A missing or invalid config, a missing required setting, or an `api_key_env` naming an unset variable is an error before the TUI starts. Shift+Tab rewrites only `permission`, preserving the rest of the file.
  Optional `[approval_description]` has a required `enabled` boolean. An absent section or `enabled = false` disables description requests. Enabling it requires a `prompt` containing `${command}` and `[approval_description.endpoint]` with `base_url`, `model`, optional `api_key_env`, and optional `parameters` under the same validation rules as the chat endpoint. The two endpoints resolve credentials independently, including when they name the same variable from `.env`.
- `.env`: read if present, only to resolve endpoint credentials; shell variables take precedence. Its values are never exported to host commands. The sandbox environment is empty. No permission checks; the app never edits `.gitignore`.
- `sessions/<name>/`: one tree per conversation; layout and `_harness_local` identity fields: `SPEC-rlm.md` Files. Every session file has required `messages` and required `_harness_local`. `messages` interleaves durable provider messages with optional local hidden-turn markers. `_harness_local` holds `repository`, `depth`, and `parent` (absent at depth 0), set at creation and never rewritten, and optional `turn_token_cache`, derived from the provider messages. Neither local form is sent to a provider. Session and local objects reject unknown fields; new local metadata must be explicit, and obsolete fields receive no compatibility handling. A new tree's name is its start time, `YYYYMMDD-HHMMSS` (local time); `/rename` changes it. Valid names are non-empty, without `/`, and don't start with `.`. The root file and its directories are created on its first save, a branch file when the branch starts. Every save writes a synced temp file and renames it over the session file.
- `replib/*.py`: optional REPL helper functions (see Helper functions). `install.sh` installs the binary (`cargo install --path . --locked`) and copies the shipped `replib/*.py` here, overwriting only those files; it never touches `config.toml`, `sessions/`, or the store.
- `repl-notes.md`: optional guidance included by `help()`.
- `failures.jsonl`: append-only app denials and unsupported Monty operations, with occurrence counts shown as display-only call notes (see Failure log).

## Messages

- The saved conversation is authoritative. The app may transform messages before saving and modify the saved conversation; requests use provider messages from turns not marked hidden. The effective system message is added separately and never saved: `[chat] prompt` for every agent, except that branches receive `[rlm] subagent_prompt` instead when it is set, verbatim, including an empty string. `--system-prompt <TEXT>` replaces `[chat] prompt` for this invocation, including resumed sessions. The argument is literal text used verbatim, including an empty string. The effective prompt is displayed and added at send time, never saved to configuration or session files.
  - user: `{"role": "user", "content"}`
  - assistant: `{"role": "assistant", "content", "tool_calls"?, ...}`; `tool_calls` entries are `{"id", "type": "function", "function": {"name", "arguments"}}`. Every other field the endpoint streamed (reasoning under whatever name it uses, and unknown fields, here and inside tool calls) is kept and sent back unchanged.
  - tool: `{"role": "tool", "tool_call_id", "content"}`
- A local entry `{"_harness_local":{"hidden":true}}` immediately after a completed turn excludes that whole turn from provider requests. Its absence means visible; unhiding removes it. A well-formed `hidden:false` entry is accepted as visible and omitted on the next save. Markers after no turn, inside a turn, or duplicated after one turn are session errors. They never enter the transcript or provider request.
- The user message is saved on send, a branch's task when the branch starts. An assistant message is saved once when its response ends (completed, Esc, error, `/exit`, or app exit), with every field that arrived, if any (besides `role`) has a value; `null`, `""`, `[]`, and `{}` don't count. A tool call without an `id` or `function.name` is dropped. A tool call still streaming when the response is interrupted or cut off at the token limit (`finish_reason: "length"`), meaning its arguments aren't valid JSON, is dropped.
- Every tool call in a saved assistant message is followed by exactly one tool message before the next request.
- Assign a normal encoded ID when a new provider call's ID first arrives; later ID fragments retain that assigned ID. Previews, saved calls, and execution results use the same ID. Existing saved IDs remain unchanged.
- All new call IDs use 32 canonical unpadded Base64url characters encoding 24 bytes: four random clear bytes followed by a 20-byte XOR-masked body. The body contains version `1`, one kind byte, one payload-length byte, up to 17 payload bytes, and random padding. The mask is the first 20 bytes of SHA-256 over `harness/tool-call-id/mask/v1\0` followed by the four clear bytes. This is obfuscation, not confidentiality. Normal calls use kind `0`; app-generated synthetic calls use kind `2`; both currently have empty payloads. Decode rejects invalid encoding, version, or payload length. No secret key, timestamp, PID, or external metadata participates.
- Messages are removed only by the per-turn cleanup (see Situational awareness).
- Display-only errors, failure-count notes, and approval descriptions are never saved in the conversation. Tool error results are saved and sent to the model.
- `--resume` without a name prints the tree names, the directories under `sessions/` holding `library/root.json`, one per line, by `root.json` modification time, oldest first, and exits without loading the config or starting the TUI. `--resume <NAME>` loads `sessions/<NAME>/library/root.json`, shows the whole conversation, and appends to the same file. An invalid name, a missing or unparseable file, or a root file not recording depth 0 without a parent is an error before the TUI starts. Any tool call without a result gets `[not run: turn stopped]`, inserted after its call's existing results and saved at once, with a line printed to the normal screen before the TUI starts (visible after exit).

## Requests

- Optional `[endpoint.parameters]` entries are forwarded as top-level JSON request fields; omitted entries are not sent. Endpoint-specific names and ranges are validated by the endpoint. Harness rejects `model`, `messages`, `tools`, `stream`, `stream_options`, and `n` as reserved keys, and rejects non-finite floats and TOML dates/times at any nesting depth. Strings, integers, finite floats, booleans, arrays, and tables retain their JSON-compatible values.
- Chat: `POST {base_url}/chat/completions` with `{"model", "stream": true, "stream_options": {"include_usage": true}, "tools": <the agent's tool definitions>, "messages"}`, plus bearer auth when a key is configured. Hidden turns and local markers are omitted. When the agent's effective system prompt is present, `messages` starts with `{"role": "system", "content": <effective prompt>}`.
- SSE `data:` lines: each `delta` is merged into the reply by the OpenAI SDK's `accumulate_delta` rule, whatever its fields: strings appended, numbers added, objects merged recursively, lists of entries with an `index` merged by matching `index` value (a fragment without one is appended), other lists extended; `index` and `type` are replaced; a value of a different type (such as a later `null`) is ignored. `role` is always `assistant`; `index` is removed from saved tool calls. `usage.total_tokens` is the agent's context count and `usage.prompt_tokens` is retained as its latest prompt count (see Situational awareness); an `error` object is an error. `[DONE]` ends the stream. EOF without `[DONE]` is an error unless a `finish_reason` was seen. A non-2xx response is an error showing status and body.

## Approval descriptions

- When enabled, each host-operation approval starts an independent, non-streaming chat-completions request to `[approval_description.endpoint]`. The request contains the configured model and parameters, one user message formed by replacing `${command}` in the configured prompt, no tools, and no primary chat history.
- `${command}` is JSON describing only the pending operation: command execution uses `{"operation":"run_command","executable":<resolved path>,"arguments":<literal argument array>,"working_directory":<resolved path>}`, and approval to use an outside command directory uses `{"operation":"use_command_directory","directory":<resolved path>}`. Enclosing REPL code and conversation content are excluded.
- The original call and approval prompt remain usable while the request runs. A display-only block beneath the call shows `Generating description…`, then `Description: <text>` or `Description unavailable: <error>`. A request failure does not resolve or disable the approval.
- Answering, canceling, or abandoning the approval removes the block and cancels unfinished generation. A result cannot attach to a later approval. Descriptions are never saved in the session or sent to the primary chat model.

## Tool calling

- Each agent's tool definitions are fixed at creation by its depth and mounts (`SPEC-rlm.md` Agents) and sent with every request; `/tools` lists the same definitions. No description names a depth.
  - `REPL`: one required string parameter `code`; description `[repl] tool_description`, loaded at startup and sent verbatim.
  - `FYI`: no parameters; description `Report the current date, the prompt-token count of the latest request, and the model name.`
  - `rlm`: required strings `task` (`The task for the subagent.`) and `name` (`A short (3-5 word) name for this subagent, shown to the user.`). Below the root its description is `Delegate a task to a subagent.` At the root it is `Delegate a task to a subagent and return its final answer. The subagent sees only your task text and works on the same repository with the following tools:`, then one `<name>: <description>` entry per depth 1 tool, rendered from the depth 1 definitions with trailing line endings trimmed, then `These tools are available only to the subagent, not to you. You cannot call them directly.`, then `Ask subagents for short answers. To hand large data from one subagent to another, have the first write it to the shared store and return the path, then include that path in the next subagent's task.`, separated by blank lines.
  - `apply_patch`, `read_file`, `grep`, `list`: their sections and Direct tool roots.
- A turn starts with a user message. Each response's tool calls are handled in order, their results are sent, and the next response is requested. The turn ends when a response has no tool calls, on Esc, or on a stream error; a branch's turn also ends on any other error. Any other error in the root's turn ends the app.
- **Situational awareness.** Each turn opens with the synthetic calls of `SPEC-rlm.md` Agents: the `FYI` tool at the root; separate `REPL` calls `FYI()`, `help()`, `store_summary()`, and, where `/library` is mounted, `library_summary()` in a branch. First, every earlier `FYI` tool call and every earlier `REPL` call whose code is made only of `FYI()`/`help()` statements (synthetic or the model's own) is removed with its result; an assistant message left with no calls, content, or other field with a value is removed too, and the session is saved. Then, after the user message, the app saves one assistant message holding the turn's synthetic calls, with synthetic-kind encoded IDs, and runs them in order like any other call, before the first request. `FYI()` is a host function implemented in `src/host.rs`; it prints the lines `FYI()`, `Date: <%a %b %d %H:%M:%S %:z %Y>`, `Prompt tokens: <count>`, and `Model: <model>`, and returns `None`. The `FYI` tool's result is the same lines without `FYI()`. The count prefers the agent's latest `usage.prompt_tokens`; when absent, it sums the context overlay's current per-turn totals and prefixes `~` if any contributing total is heuristic. Honesty rule: a synthetic call is a real call the model could make itself, so an explicit call works the same way (an `FYI` token count is the latest at call time).
- **Approval exemption contract.** All synthetic tool calls, present and future, including the `FYI` tool, `FYI()`, `help()`, `store_summary()`, and `library_summary()`, never require approval, whether generated by the app or explicitly called by the model. This holds at every sandbox permission level. Code consisting only of these calls is exempt; unrelated code bundled with them, including code evaluated in arguments, does not inherit the exemption and follows normal permission and approval rules.
- `reasoning()` is not a synthetic tool call and has a separate approval exemption. Code that is exactly `reasoning(<one string literal>)` (optional `r`/`u` prefix, any quoting; not f-strings, bytes, concatenation, keyword arguments, or other code alongside) is answered without approval or execution: its result is `(no output)` at every permission level, since `reasoning()` is a no-op the model is meant to call freely and the REPL's `reasoning` may have been rebound. Its exemption never extends to unrelated code.
- REPL calls run in Monty except for the exact `reasoning()` exemption; individual filesystem and host-command operations enforce the permissions and approvals under REPL and Host commands. At a host-operation prompt, `y` allows that operation and `n` raises a permission error at the call. `apply_patch`, `read_file`, `grep`, and `list` run directly in Rust; `FYI` answers without running code.
- A call naming a tool the agent does not offer, including another depth's tool, returns `Unknown tool: <name>` without running. When the agent has a REPL and the name is one of its host functions or a helper function loaded in its REPL, the result is `Unknown tool: <name>. <name> is a REPL function, not a tool; call it inside REPL code: <signature>`, the signature being a host function's `help()` stub line without `: ...` or a helper's signature line as `help()` prints it.
- Malformed arguments return error results without running. `REPL` requires a string `code`; `apply_patch` requires a string `patch`; `read_file` requires a string `path`; `grep` requires a string `pattern`; `rlm` requires strings `task` and `name`; `FYI` requires a JSON object, whose members are ignored. A direct tool's `root` must be a string or `null`.
- Result: printed output in order, then the repr of a trailing expression if not `None`, then the exception type and descriptive message without traceback frames if the code raised. Host commands return captured stdout and stderr rather than printing them. Every tool result, including an `rlm` result, is `(no output)` if empty and is truncated at `output_limit` bytes on a character boundary, followed by `[output truncated: N more bytes]`.
- An accepted `rlm` call creates the branch session with `task` as its user message and opens the branch's turn; the parent waits, and the branch's final response content is the call's result. A branch whose session cannot be created gives the result `[branch failed: <error>]`. A branch whose turn ends on an error fails: its turn stops as Esc stops one, the error is kept out of its file, and the result is its response text so far (its complete final response, when the failure came after it) followed by `[branch failed: <error>]` on its own line. A failed save while stopping it records that result, then ends the app.
- Esc (or exit) stops every agent on the stack, deepest first. While code runs, it discards the REPL worker and kills any active host command's process group; the result is the output so far plus `[stopped by user]`. Every complete call that never ran, including an `rlm` call that never got a branch, gets `[not run: turn stopped]`. The deepest agent's response text so far (its complete final response, when stopped after it) followed by `[stopped by user]` on its own line is its parent's `rlm` result and passes up unchanged as each ancestor's.
- SIGHUP, SIGTERM, and SIGINT stop every agent the same way (recording only, no transcript output) and exit.

## Direct tool roots

- `read_file`, `grep`, `list`, and `apply_patch` take an optional string `root`; omitted or `null` means `repository`, and `store` and `library` name the roots of `SPEC-rlm.md` Store and Library. Paths, containment, results, and errors are relative to the named root. The permission level governs only the repository: store reads and writes and library reads are allowed at every level.
- A definition gains `root` only when the agent reaches a root besides the repository: an enum of the agent's roots with default `repository`, and a description note listing each with its access rule. `apply_patch` offers only writable roots, never `library`.
- A call naming a root the agent lacks, or any other value, returns `<tool>: root "<name>" is not available` without running. `apply_patch` naming `library` where the agent has it returns `Not applied: root "library" is read-only`.

## Direct read tool

- `read_file` accepts a root-relative `path` and optional positive integer `start_line` and `end_line`. Bounds are 1-based and inclusive; omitted bounds mean line 1 and EOF. LF separates lines; CRLF endings are recognized, and bare CR/TAB characters are data. Return selected UTF-8 lines prefixed by their original line number and a tab. EOF ends the read normally.
- Reads run directly in Rust through the named root's `cap-std` directory capability. Repository reads require `read-only` or `read-write`; `none` denies access. Reject absolute paths, absolute symlink targets, and escaping resolution. Ordinary filesystem and UTF-8 errors become tool results. The existing result handling and `output_limit` apply.
- Calls execute in the ordered tool queue; path/range arguments and results are shown and saved. Esc discards the pending result and records `[stopped by user]`; cancellation and permission changes are checked between line reads.

## List tool

- `list` accepts optional string `path="."`, boolean `recursive=false`, and string `include=""`. Path identifies a root-relative directory; omitted means the root itself. The case-sensitive basename glob filters emitted entries, including directories, without pruning recursive traversal. Empty means no filter; invalid globs return errors. Hidden entries are included.
- Read entry metadata without opening file contents. Sort by underlying root-relative path and emit one row per entry: `path  size  mtime`. Quote and escape paths, append `/` to directory names, and label symlinks `[symlink]` and other special entries `[special]`. Size is exact metadata bytes, including directories' own sizes rather than recursive totals. Mtime is UTC with nine fractional-second digits.
- Resolve the starting directory through the root's capability. Recursive descents use opened directory handles and never follow symlinks, including replacement races. Symlink rows use the link's own size and mtime without reading its target. Absolute input paths, absolute symlink resolution, and root escapes are rejected.
- Metadata and traversal errors precede successful rows; other entries continue where possible. Repository listings require `read-only` or `read-write`; `none` denies access. Check permission and cancellation between metadata operations and before returning output. Esc discards the pending result and records `[stopped by user]`. Use the ordered tool queue, structured argument display, session persistence, empty-result handling, and existing `output_limit`.

## Grep tool

- `grep` requires a string `pattern`. Optional arguments: string `path="."`, boolean `fixed=false`, boolean `ignore_case=false`, nonnegative integer `context=0`, and string `include=""`. Path is a literal root-relative file or directory; omitted means the root itself. There is no command parsing or stdin interpretation.
- Matching uses ripgrep's line-oriented extended regex engine; `fixed` selects newline-separated literal strings and `ignore_case` enables case-insensitive content matching. `include` is a case-sensitive glob matched against file basenames at every depth, including an explicitly named file; empty means no filter. Invalid regexes and globs return errors. Directory traversal is not pruned by the filename filter.
- Directories are always searched recursively, including hidden and ignored files. Skip symlinks encountered during traversal and explicitly named directory symlinks, reporting `grep: warning: <relative path>: skipped symlink` in the tool result. Direct file operands may follow contained relative symlinks.
- Search regular files only. Skip named pipes, sockets, and devices with a brief warning naming the relative path. Open candidates without blocking and check the opened handle's type before reading.
- Use the named root's `cap-std` capability for traversal and file opens; pass opened readers to ripgrep. Repository searches require `read-only` or `read-write`; reject absolute paths, absolute symlink targets, and escaping resolution. Generated filename labels and diagnostics remain root-relative.
- Matching and context lines always use `file:line:text`, with 1-based line numbers. `context` sets both leading and trailing counts; overlapping groups merge and disjoint groups use `--` separators. Discard all output from files where NUL bytes are detected. Other output bytes use the host-command UTF-8 replacement policy. Errors and skip warnings precede matches; traversal continues after file errors. The existing empty-result handling and `output_limit` apply.
- Execute in the ordered tool queue. Display argument objects during streaming and replay; save calls and results normally. Historical arguments remain unchanged. Check cancellation and permission changes at traversal, read, and output boundaries. Esc discards the pending result and records `[stopped by user]`.

## Direct patch tool

- `apply_patch` is a function tool with one required JSON string `patch`, plus the optional `root` of Direct tool roots. Decode once and execute directly in Rust, in the same ordered call queue as `REPL`. Complete calls/results persist and resume through the existing session format; partial JSON is never executed.
- Envelope: `*** Begin Patch` / `*** End Patch`. Operations: `*** Add File: path` with `+` content lines, `*** Replace File: path` with `+` content lines replacing the whole text of an existing file, `*** Delete File: path`, and `*** Update File: path` with optional `*** Move to: path`. Update hunks use `@@`, `@@ exact anchor line`, or `@@ line N: exact anchor line` on one line; the anchor is outside the replacement. The `line N: ` header prefix is reserved for numbered anchors; `N` is a positive integer and the text is required. Prefixes are space for context, `-` for removal, `+` for insertion. Context/removal lines define the old text; context/addition lines define the new text, in order. `*** End of File` constrains the hunk to the file end. Decode JSON escaping once; patch content then remains literal.
- Each update's context/removals must match exactly and uniquely against its input contents. An anchor constrains matching to locations after an exact anchor line; the complete match determines uniqueness. A numbered anchor requires input line `N` to equal the anchor text exactly and constrains matching to locations after it; a different line `N` or a line beyond the input is an error reporting the actual text and where the anchor text occurs. Numbers refer to the update's input contents, so earlier hunks in the same update do not shift them. Changed ranges cannot overlap within an update; shared unchanged context is allowed. Insertion with no old lines or anchor appends at EOF.
- Use the named root's `cap-std` directory capability: the repository, rooted at launch, requires `read-write`; the store is writable at every level. Reject all absolute paths. Path resolution cannot escape the root or follow absolute symlink targets. Text updates follow relative symlinks within the root, preserving the links; rename and deletion act on named entries. Add and rename destinations are checked for absence during validation, including dangling symlinks. Filesystem errors propagate without creating missing directories or copying across filesystems.
- Preserve untouched bytes, LF/CRLF line endings, and existing final-newline state. Bare CR is content, matching `read_file`; CR-only files have one logical line. Inserted lines use the first existing ending, or LF when none exists; appending after an unterminated line supplies the necessary separator. New files use LF. Replacement is a text update: the file must exist and be UTF-8, follows relative symlinks, and takes the existing file's ending and final-newline state for every content line; no old text is matched. Atomic text replacement retains source permission bits but creates a new inode; other metadata and hard-link relationships are not copied. Rename-only operations use filesystem rename without reading contents and preserve the original entry.
- Parse the complete patch before filesystem changes. Then validate, stage, and commit each operation in submitted order against the current filesystem. Any validation, staging, commit, or synchronization failure stops subsequent operations and reports earlier commits; cleanup failures are reported. Replacements are atomic per file, not transactional across operations. A combined update/move edits through the source path, then renames the named entry.
- Cancellation and permission revocation stop subsequent operations. An in-flight filesystem operation finishes accounting before the writer is joined and its result saved; no writer remains detached after a stopped turn or orderly exit.
- Results put committed/uncompleted operations before changed regions with fresh line numbers and two neighboring lines. Matching errors include bounded source context and candidate locations. The existing `output_limit` applies to the entire result, including accounting, with an explicit truncation notice.

## Context display and token counts

- The upper-right overlay is the context editor of the agent on screen. It projects that session's durable provider messages into turn rows without message-number labels. User content, assistant content, reasoning, and tool calls are separate rows. A tool call and its result are coupled as one row. Empty assistant group headers remain for ordinary calls; app-generated synthetic calls (synthetic ID kind) and their empty groups are omitted. Turn labels use `theme.context_turn`; turn counts use `theme.context_tokens`; role labels use `theme.context_role`; row counts use `theme.context_message_tokens`; previews and separators use `theme.status`.
- Clicking any displayed row toggles its completed turn between model-visible and hidden, saves the session immediately, and discards the agent's reported usage until a reply reports it again. The active turn cannot be toggled. Every text segment belonging to a hidden turn is rendered in the context editor at half its configured foreground RGB values. The main transcript remains unchanged and displays every turn.
- Each turn has a non-position header with its complete turn-slice token count. Each position shows its marginal count: tokenize progressive valid prefixes within that turn and subtract the preceding prefix. Hidden synthetic prefixes advance the baseline without creating positions. The final prefix is the turn total. Hidden turns retain these individual counts but do not contribute to aggregate fallback counts in the status line or `FYI`.
- Derive the tokenizer URL by removing a final `/v1` from `[endpoint] base_url` and appending `/tokenize`. Requests use the configured model and credentials with `add_generation_prompt: false`.
- Cache results under `_harness_local.turn_token_cache`. Each digest covers the algorithm version, tokenizer URL, model, and exact durable turn slice. Reuse unchanged entries; calculate only missing or changed completed turns, sequentially, so a branch, whose one turn is active while it is on screen, gets no cache. Missing counts immediately display deterministic estimates while one provider attempt runs. If any prefix request fails, estimate every prefix in that turn as `ceil(compact-prefix-JSON UTF-8 bytes * 4 / 13)`, cache the result, and prefix its displayed counts with `~`.
- The expanded overlay is as wide as its widest row and at least its full header, capped at nine-sixteenths of transcript width, at most nine-sixteenths of transcript height, and scrolls under the mouse. Its top border centers the header `[Context][<root>]…[<on-screen agent>]` (names and collapse under the cap: `SPEC-rlm.md` Human control) in `theme.assistant`, with `[-]` at the right; it is not drawn when the width cap cannot fit `[Context][…]` with the corners and toggle, or the height cap three rows (one collapsed). Collapsing retains the same one-row header and width with `[+]`; expanding restores the retained scroll position. It covers transcript cells without changing transcript wrapping or input dimensions and consumes its own mouse events.

## REPL

- Monty runs in workers of the harness binary, managed by one pool of at most `[rlm] max_depth` workers. An agent's first `REPL` call of a turn checks out a fresh session and loads the helper functions. State persists across that turn's calls; normal turn completion finishes the session and returns the worker. Cancellation discards it. A parent keeps its session while its branch runs; no agent sees another's session.
- Static type checking is disabled. Ordinary runtime exceptions stop the snippet where they occur while preserving completed effects and surviving state. There is no execution timeout; Esc stops execution. The worker enforces `[repl] max_memory_mb` with no suspension limit.
- A worker crash or memory-limit failure ends the call with a message stating why the worker restarted and that every variable, import, and function defined earlier in the turn was lost. The next call starts a fresh session and reloads the helper functions without replaying failed code.
- The repository is mounted at the virtual path `/repository`, which is also the sandbox working directory, so sandbox paths never show its host path. `/store` and `/library` are mounted where `SPEC-rlm.md` gives them. Host directories mounted together must not coincide or nest; Monty rejects such mounts, so REPL calls fail. The sandbox environment is empty: `os.environ` is `{}` and `os.getenv()` returns its default.
- Every filesystem operation is authorized at the operation boundary by the mount serving its path, the longest matching virtual path. Under `/repository` the current permission applies: `none` denies reads and writes, `read-only` permits reads only, and `read-write` permits both; a permission change therefore affects the next operation, including within a running call. `/store` permits reads and writes, `/library` reads only. A rename authorizes both endpoints.
- Filesystem containment follows the resolved operation rather than rejecting absolute paths or `..` components solely by spelling. Operations resolving outside every mount are denied at every permission level without an approval path. Monty rejects absolute symlink targets, including targets back inside the repository; predicates may return `False` for rejected paths. `Path.resolve()` performs lexical normalization rather than host symlink resolution.
- App denials raise a catchable `PermissionError` at the operation. Expected bad input and filesystem failures use specific Python exception types and messages; tracebacks are reserved for unexpected bugs.

## Host commands

- `run(argv, cwd=None)` requires a nonempty list of strings whose first item is a command name or a relative path containing `/` to an executable inside the repository. A path must contain no `..` component and resolves against `cwd`, following symlinks: a result outside the repository is denied, a missing one raises `FileNotFoundError`, and one that is not a regular file with an execute bit raises `PermissionError` without being logged as a denial. A path never consults `[commands]` and requires approval for its resolved executable and literal arguments on every call. Errors name the path as spelled. It returns `{"exit_code": <integer>, "stdout": <string>, "stderr": <string>}` and prints nothing. A signal exit uses the negative signal number.
- A name in `[commands] deny` is always rejected. A name in `[commands.allow]` runs the configured absolute executable without approval. Any other name is resolved through Harness's startup `PATH` and requires approval for its resolved executable and literal arguments; an unresolved name raises `FileNotFoundError`. Command authorization is independent of the REPL filesystem permission.
- `cwd` defaults to the repository root, independently of sandbox `os.chdir()`. An explicit value must be relative and contain no `..` component. It resolves against the repository root; if symlink resolution reaches an outside directory, using that resolved directory requires a separate approval.
- Commands run directly without a shell, interactive stdin, terminal, or controlling terminal. They receive Harness's environment minus the exact names in `[commands] env_filter`; they are host processes outside the Monty mounts, so the virtual `/repository`, `/store`, and `/library` paths do not exist for them. Output is captured until completion and decoded as UTF-8 with replacement. There is no timeout; Esc kills the command's process group.

## Helper functions

- `replib/*.py` files execute in filename order at the start of each fresh session under normal REPL operation permissions. Directory, read, parse, and evaluation failures appear in the first call's output. Other files continue loading after ordinary failures; a worker-loss failure ends loading with the same state-loss behavior as any call.
- Public functions are top-level definitions whose names do not begin with `_`. Harness reads their source signatures and docstrings without executing them on the host. Only functions from files that loaded successfully appear in `help()`.
- `help()` takes no arguments and prints `help()`, the output limit, and optional `repl-notes.md`, then these sections. `Files:` the repository with the live permission level, plus `/store` and `/library` where mounted, each with its access and how the REPL and the direct tools reach it. `Python:` the supported subset, what is unavailable, and what to use instead. Its `Use instead` lines, one `<names>: <alternative>` line per alternative with its unsupported names comma-separated, are generated from one table of unsupported names, each with its alternative and any exact Monty exception types and messages that identify it, and a REPL call whose final exception exactly matches one of those gets one more line after it: `<name> is not available in this REPL; use <alternative>.` `REPL functions`: the host function stubs, `store_summary` and `library_summary` only where their mount is, noting they are not tools. `Commands:` `run()`'s contract, the allow-listed names not denied, the denied names, and that any other command on `PATH` or repository executable asks for approval on every call. `Helper functions:` loaded public helper functions with signatures and docstrings, or `No helper functions loaded.` `lib_source(name)` returns the current contents of `replib/<name>.py`.
- `store_summary()` and `library_summary()` exist only where their mount is. Each prints `<mount>: empty` when the mount holds no regular files, otherwise `<mount>: <N> files, <B> bytes, last updated <newest mtime, in FYI's date format>` and an indented `Recent:` line of up to five mount-relative paths, newest first. The walk includes hidden files, follows no symlinks, and counts no special files; its first failure raises, naming the virtual path.
- The shipped `reasoning(text)` is a no-op that prints nothing and returns `None`; the `reasoning()` approval exemption depends on that behavior. `glob(pattern, path=None, exclude=())` returns sorted relative paths. `search(pattern, path=None, glob=None, exclude=())` returns records containing `path`, 1-based `line`, and `text`; it skips files containing a NUL byte, as the `grep` tool does, and decodes every other file as UTF-8.
- `glob` and `search` default their root to `.`, preserve the supplied relative root in results, skip only the entry names in `exclude`, and reject absolute paths and `..` components in path inputs; `**` does not enter directory symlinks. A missing or non-directory root, a `str` `exclude`, and unexpected read, traversal, and decoding errors raise instead of silently producing empty or partial results.

## Failure log

- App denials and unsupported Monty operations append one JSON object per line to `failures.jsonl`: `{"time", "session", "kind", "message", "code"}`, where `session` is the tree directory name for the root and the call ID for a branch. Accepted `kind` values are `denied`, `unsupported`, and the retained `type check` category. The timestamp is RFC 3339 UTC. Each record is persisted before its in-memory count advances.
- Startup validates every existing record, including its fields, timestamp, and kind; malformed history is a startup error. Occurrence counts are keyed by `(kind, first line of message)` across sessions.
- Each new failure adds a display-only note under its call: `<kind>: <first line> (seen N times)`, including the current occurrence. Notes are never saved in session messages or sent to the model; the call's error result is saved normally.

## Terminal UI

- Required `[theme]` foreground colors: `system`, `user`, `assistant`, `reasoning`, `tool_call`, `tool_result`, `approval`, `warning`, `error`, `status`, `context_turn`, `context_tokens`, `context_role`, `context_message_tokens`, `input`. Every value must be a `#RRGGBB` string; names, missing fields, and unknown keys are startup errors. Colors load at startup without terminal dim modifiers or background overrides; the context editor derives half-bright hidden-turn colors from them. Selection remains reverse video.
- Full screen on the alternate screen, with mouse capture. On exit the terminal returns to its previous contents; nothing is left behind.
- The transcript draws the on-screen agent's complete durable conversation, including hidden turns, from its effective system message, provider-message entries, and the turn in progress. Local turn markers are not drawn. Fields other than content, reasoning, and tool calls are sent when their turn is visible but are not shown.
  - System message (if any): `system`, then the prompt, both in `theme.system`.
  - User message: `> text`, in `theme.user`.
  - Assistant: reasoning in `theme.reasoning`, a blank line, then content in `theme.assistant`. Reasoning is the first non-empty of `reasoning`, `reasoning_content`, and the `text` or `summary` of each `reasoning_details` entry (joined by blank lines).
  - Tool call: tool name and payload in `theme.tool_call`, any call notes, then the result in `theme.tool_result`. The payload is the decoded code for `REPL`, the patch text for `apply_patch`, preceded by a `root: <value>` line when the arguments include `root` (a non-string value as its JSON text), and the raw arguments for `read_file`, `grep`, `list`, `rlm`, and `FYI`. Calls appear once their name and id arrive; the payload preview grows as arguments stream in, decoding complete JSON string characters for code/patch text and waiting for incomplete escapes; a streaming patch shows its `root` line once a complete `root` value precedes the patch. Malformed arguments are shown raw. Previewing changes neither execution nor interrupted-call persistence. Running REPL print output appears live, in full; host command output is captured until completion.
  - A blank line follows each message and each tool call.
  - Notes are shown but never saved, and belong to the agent whose transcript shows them: they end when that branch ends, when `/new` replaces the root, or on exit. Failure notes use `theme.warning` under their call, and errors (`error: ...`) use `theme.error` where they happened. Call notes disappear when their call is removed; errors keep their place among the remaining messages.
- Word-wrapped by the app to the screen width and re-wrapped on resize. Rows break after whitespace; longer words break at the width. Tabs expand to 8-column stops; other control characters are not drawn.
- Scrolling: while at the bottom the view follows new output; scrolled up, it stays on the same text as output arrives or the width changes, until scrolled back to the bottom. Sending a message, or a change of the agent on screen, returns to the bottom.
- Selection: dragging selects transcript text (reverse video); dragging onto the top row or below the transcript scrolls one row per mouse event. Releasing copies the selection's source text (without wrap breaks) with OSC 52. The selection stays until the next click or key press. A failed copy shows the notice `couldn't copy: <error>`.
- Each frame is one synchronized update.
- The context overlay is specified under Context display and token counts.
- Bottom region: a top rule in `theme.status`, a `> ` prompt and input text in `theme.input` (word-wrapped, growing up to half the screen height), and a status line, or the command completion list in its place (see Commands); the region grows to fit the list. Completion labels use `theme.status`, with the selected row reversed. A click there only clears the selection.
- Status line shows the on-screen agent's name (`SPEC-rlm.md` Human control), the model, token count, and permission level. The token count prefers that agent's latest reported `usage.total_tokens`; when absent, it sums the context overlay's current per-turn totals and prefixes `~` if any contributing total is heuristic. It uses `theme.status`, notices use `theme.warning`, and a pending host operation uses `theme.approval` with `allow <operation>? (y/n, esc to stop)`. Approval-description display behavior is specified under Approval descriptions.
- Bracketed paste is enabled. Pasted text is inserted as-is (CR and CRLF become LF) and never sends.

## Keys

| Key | Action |
|---|---|
| Enter | Send (ignored when blank); with slash completion open, run the highlighted command (or complete one that takes an argument; see Commands) |
| ^J | Insert newline |
| ^K | Delete to end of line; at end of line, join the next line |
| ^U | Delete to start of line; at start of line, join the previous line |
| Esc | Stop the turn and every running branch; with the completion list open, only close it |
| Shift+Tab | Cycle filesystem permission: none → read-only → read-write → none; saved to configuration |
| Up / Down, Tab | With a completion list open: move the highlight (wrapping), complete the highlighted command |
| y / n | Answer an approval prompt (while it shows, only y, n, Esc, Shift+Tab, PageUp/PageDown, and the mouse act) |
| PageUp / PageDown | Scroll the transcript a screen, less two rows |
| Mouse wheel | Scroll the transcript one row |
| Mouse drag | Select transcript text; copied on release |
| ^C, ^D | Nothing |
| Others | ratatui-textarea defaults |

## Commands

- Input starting with `/` is a command. A leading space sends a literal `/`.
- Completion list: shown below the input in place of the status line while the input is one line starting with `/` without whitespace, some command's name or alias starts with it, and no approval prompt is showing. One row per matching command (by name if it matches, else by its first matching alias) with its description; the first is highlighted, and any edit highlights the first again. At most 8 rows, scrolling with the highlight. Esc closes it until the input changes. Commands are defined once, in `src/commands.rs`, for both running and completion; the list order puts `/exit` last.
- A command is its name or alias, then optionally whitespace and an argument (trimmed). Only commands that take an argument match input with one. Enter on a highlighted command that takes an argument completes it followed by a space instead of running it.
- `/rename <name>`: rename the tree directory to `sessions/<name>/`, with every session file in it (before the first save, only the name it will be created under). While a branch runs it shows the notice `couldn't rename: a subagent is running`; an invalid or existing name shows `couldn't rename: <error>`. Both keep the input; the current name is a no-op.
- `/tools`: show each tool's name and full description for the agent on screen, including the configured REPL description, using the same definitions as its requests. Output is scrollable and copyable in the conversation display, without a model request or session save; it is a note (see Terminal UI). No arguments.
- `/new`, `/clear`: replace the root with a new root session as at launch, named by its start time and created at its first save; the previous tree stays on disk. The display starts over and a collapsed context overlay reopens. While any turn runs it shows the notice `turn in progress (esc to stop)`; a failure shows `couldn't start a new session: <error>`. Both keep the input. No arguments.
- `/exit`, `/quit`: exit.
- Unknown command: notice `unknown command: <input>`; the input is kept.
- During a turn, the input stays editable and commands run (`/new` refuses, and `/rename` refuses while a branch runs). Enter on a message shows the notice `turn in progress (esc to stop)` and keeps the draft.

## Source

| File | Role |
|---|---|
| `src/main.rs` | Arguments, worker dispatch, session listing, config and session loading |
| `src/app.rs` | Event loop, agent stack (branch start, answer, failure, Esc cascade), display of the on-screen agent, notes, commands, status line |
| `src/agent.rs` | One agent: session, turn state machine, tool set, streaming, ordered tool queue, `rlm` delegation, turn-start calls and cleanup, direct-tool roots |
| `src/root.rs` | Repository, store, and library roots; direct-tool access; tree library handles |
| `src/tui.rs` | Alternate screen, mouse capture, screen layout, context overlay and header, synchronized updates, clipboard |
| `src/transcript.rs` | Transcript view: word wrap, scrolling, selection |
| `src/input.rs` | Textarea setup, key map, paste |
| `src/commands.rs` | Command table: names, aliases, descriptions, actions, arguments |
| `src/client.rs` | Tool definitions by depth and mounts, streaming request, SSE parsing and delta merging, tokenizer and approval-description requests |
| `src/token_count.rs` | Per-turn and per-position token counts, tokenizer queue, cache, estimates |
| `src/patch.rs` | Direct patch lifecycle, root selection, sequential execution, cancellation, and commit receipts |
| `src/patch/engine.rs` | Literal patch parsing, exact matching, and byte-preserving text updates |
| `src/patch/fs.rs` | Independent root confinement, staging, and file commits |
| `src/read.rs` | Direct numbered file reads, line ranges, and permission/cancellation checks |
| `src/list.rs` | Directory metadata, basename filtering, sorted output, confined traversal and cancellation |
| `src/grep.rs` | Structured search arguments, filename filtering, confined traversal, ripgrep search and formatting, permission/cancellation checks |
| `src/repl.rs` | Monty pool and per-turn session lifecycle, mounts by depth, suspension handling, helper-function loading, recovery |
| `src/worker.rs` | Monty worker protocol and memory limits |
| `src/host.rs` | Host functions (`FYI`, `help`, `run`, `lib_source`, mount summaries), mounts by depth, command execution and approvals, helper-function metadata |
| `src/unsupported.rs` | Unsupported-name table: `help()`'s `Use instead` lines and REPL exception pointers |
| `src/hostfs.rs` | Per-operation filesystem authorization by mount, empty sandbox environment |
| `src/failures.rs` | Failure-log persistence and occurrence counts |
| `src/config.rs` | Harness directory, configuration and endpoint credentials, effective system prompts, permission persistence |
| `src/session.rs` | Message types, session tree layout, file I/O and local metadata, hidden-turn markers, listing, renaming, call removal |
| `src/tool_reasoning.rs` | The `reasoning()` approval exemption |
| `src/tool_call_id.rs` | Shared encoded call IDs and metadata decoding |
| `replib/reasoning.py` | `reasoning()` helper function |
| `replib/glob.py` | Sorted relative-path globbing |
| `replib/search.py` | Regex file search with path, line, and text results |
| `install.sh` | Binary install and shipped `replib/*.py` copy |
