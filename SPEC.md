# harness

Terminal chat client for an OpenAI-compatible chat completions endpoint, with one tool: a sandboxed Monty Python-subset REPL. Read `SPEC-sandbox.md` for the detailed runtime, permissions, host-command, library, failure-log, and approval-description contracts.

## Files

All paths are relative to the harness directory, `~/.harness` (home directory from the OS; not found is an error before the TUI starts), shared by every working directory. The launch working directory is the repository root for sandbox filesystem access and the default host-command working directory.

- `config.toml`:
  ```toml
  [endpoint]
  base_url = "http://host:8000/v1"   # requests go to {base_url}/chat/completions; trailing `/` trimmed
  model = "your-model-name"
  api_key_env = "OPENAI_API_KEY"     # optional: names the variable holding the key; omitted = no Authorization header

  [chat]
  prompt = """..."""                 # chat system message; omitted = none
  tool_reasoning = false             # see Tool reasoning; rewritten by /tool-reasoning

  [repl]
  tool_description = "..."           # sent verbatim in the REPL tool definition
  permission = "none"                # "none" | "read-only" | "read-write"; rewritten by Shift+Tab
  max_memory_mb = 2048               # worker memory limit in MiB
  output_limit = 100000              # bytes of output per call sent to the model

  [commands]
  deny = ["git", "rm"]               # command names; deny takes precedence over allow
  env_filter = ["GITHUB_TOKEN"]      # exact names removed from command environments

  [commands.allow]                   # command name = absolute executable path
  ```
  All settings shown are required except `api_key_env` and `chat.prompt`; the values above are examples, not runtime defaults. The required `[theme]` table is specified under Terminal UI. Keep `[commands.allow]` even when empty; its values must be absolute paths. Unknown keys are rejected. A missing or invalid config, a missing required setting, or an `api_key_env` naming an unset variable is an error before the TUI starts. Shift+Tab rewrites only `permission`, and `/tool-reasoning` only `tool_reasoning`, preserving the rest of the file.
  Optional `[approval_description]` configuration is specified in `SPEC-sandbox.md`; absent or disabled means no description requests.
- `.env`: read if present, only to resolve endpoint credentials; shell variables take precedence. Its values are never exported to host commands. The sandbox environment is empty. No permission checks; the app never edits `.gitignore`.
- `sessions/<name>.json`: `{ "messages": [...] }`. A new session's name is its start time, `YYYYMMDD-HHMMSS` (local time); `/rename` changes it. Valid names are non-empty, without `/`, and don't start with `.`. Created on first save. Every save writes a synced temp file and renames it over the session file.
- `replib/*.py`: optional REPL function library (see REPL).
- `repl-notes.md`: optional guidance included by `help()`.
- `failures.jsonl`: app denials and unsupported Monty operations, with occurrence counts shown as display-only call notes; format and counting rules are specified in `SPEC-sandbox.md`.

## Messages

- `messages` is the saved conversation. Requests use it after applying the optional Tool reasoning conversion and prepending the effective system message. `--system-prompt <TEXT>` overrides `[chat] prompt` for this invocation, including resumed sessions. The argument is literal text used verbatim, including an empty string. Without it, the configured prompt is used. The effective prompt is displayed and added at send time, never saved to configuration or session files.
  - user: `{"role": "user", "content"}`
  - assistant: `{"role": "assistant", "content", "tool_calls"?, ...}`; `tool_calls` entries are `{"id", "type": "function", "function": {"name", "arguments"}}`. Every other field the endpoint streamed (reasoning under whatever name it uses, and unknown fields, here and inside tool calls) is kept; outgoing reasoning fields change only through Tool reasoning conversion.
  - tool: `{"role": "tool", "tool_call_id", "content"}`
- The user message is saved on send. An assistant message is saved once when its response ends (completed, Esc, error, `/exit`, or app exit), with every field that arrived, if any (besides `role`) has a value; `null`, `""`, `[]`, and `{}` don't count. A tool call without an `id` or `function.name` is dropped. A tool call still streaming when the response is interrupted or cut off at the token limit (`finish_reason: "length"`), meaning its arguments aren't valid JSON, is dropped.
- Every tool call in a saved assistant message is followed by exactly one tool message before the next request.
- Messages are only removed by the per-turn `FYI()`/`help()` cleanup (see Tool calling).
- Display-only errors, failure-count notes, and approval descriptions are never saved in the conversation. Tool error results are saved and sent to the model.
- `--resume` without a name prints the session names, one per line, by file modification time, oldest first, and exits without loading the config or starting the TUI. `--resume <NAME>` loads `sessions/<NAME>.json`, shows the whole conversation, and appends to the same file. An invalid name or a missing or unparseable file is an error before the TUI starts. Any tool call without a result gets `[not run: turn stopped]`, inserted after its call's existing results and saved at once, with a line printed to the normal screen before the TUI starts (visible after exit).

## Requests

- Optional `[endpoint.parameters]` entries are forwarded as top-level JSON request fields; omitted entries are not sent. Endpoint-specific names and ranges are validated by the endpoint. Harness rejects `model`, `messages`, `tools`, `stream`, `stream_options`, and `n` as reserved keys, and rejects non-finite floats and TOML dates/times at any nesting depth. Strings, integers, finite floats, booleans, arrays, and tables retain their JSON-compatible values.
- Chat: `POST {base_url}/chat/completions` with `{"model", "stream": true, "stream_options": {"include_usage": true}, "tools": [REPL], "messages"}`, plus bearer auth when a key is configured. When an effective system prompt is present, `messages` starts with `{"role": "system", "content": <effective prompt>}`.
- SSE `data:` lines: each `delta` is merged into the reply by the OpenAI SDK's `accumulate_delta` rule, whatever its fields: strings appended, numbers added, objects merged recursively, lists of entries with an `index` merged by matching `index` value (a fragment without one is appended), other lists extended; `index` and `type` are replaced; a value of a different type (such as a later `null`) is ignored. `role` is always `assistant`; `index` is removed from saved tool calls. `usage.total_tokens` is the context count and `usage.prompt_tokens` is retained for the next turn's FYI snapshot, an `error` object is an error. `[DONE]` ends the stream. EOF without `[DONE]` is an error unless a `finish_reason` was seen. A non-2xx response is an error showing status and body.
- Optional approval descriptions use an independent, non-streaming endpoint request containing only the configured prompt with the pending operation substituted. Configuration, request contents, cancellation, and display behavior are specified in `SPEC-sandbox.md`.

## Tool calling

- The REPL tool definition:
  - name `REPL`, one required string parameter `code`
  - description: `[repl] tool_description`, loaded at startup and sent verbatim.
- A turn starts with a user message. Each response's tool calls are handled in order, their results are sent, and the next response is requested. The turn ends when a response has no tool calls, on Esc, or on a stream error.
- **Situational awareness.** Each turn opens with synthetic calls. First, every earlier `REPL` call whose code is made only of `FYI()`/`help()` statements (synthetic or the model's own) is removed with its result; an assistant message left with no calls, content, or other field with a value is removed too, and the session is saved. Then, after the user message, the app saves an assistant message with two `REPL` calls, `{"code": "FYI()"}` and `{"code": "help()"}` (ids `fyi-<unix millis>-<counter>`), and runs them in the REPL like any other calls, before the first request. `FYI()` is a host function implemented in `src/host.rs`; it prints the lines `FYI()`, `Date: <%a %b %d %H:%M:%S %:z %Y>`, `Prompt tokens: <latest usage.prompt_tokens>` (omitted until a response reports usage), and `Model: <model>`, and returns `None`. Honesty rule: a synthetic call is a real call the model could make itself, so an explicit `FYI()` works the same way (its token count is the latest at call time). `help()` lists `FYI()` among the host functions.
- **Approval exemption contract.** All synthetic tool calls, present and future, including `FYI()` and `help()`, never require approval, whether generated by the app or explicitly called by the model. This holds at every sandbox permission level. Code consisting only of these calls is exempt; unrelated code bundled with them, including code evaluated in arguments, does not inherit the exemption and follows normal permission and approval rules.
- `reasoning()` is not a synthetic tool call and has a separate approval exemption. Code that is exactly `reasoning(<one string literal>)` (optional `r`/`u` prefix, any quoting; not f-strings, bytes, concatenation, keyword arguments, or other code alongside) is answered without approval or execution: its result is `(no output)` at every permission level, since `reasoning()` is a no-op the model is meant to call freely and the REPL's `reasoning` may have been rebound. Its exemption never extends to unrelated code.
- Calls run in Monty; individual filesystem and host-command operations enforce the permissions and approvals specified in `SPEC-sandbox.md`. At a host-operation prompt, `y` allows that operation and `n` raises a permission error at the call.
- A call that isn't `REPL`, or whose arguments aren't JSON with a string `code`, returns an error result without running.
- Result: printed output in order, then the repr of a trailing expression if not `None`, then the runtime error if the code raised; `(no output)` if empty. Host commands return captured stdout and stderr rather than printing them. Truncated at `output_limit` bytes on a character boundary, followed by `[output truncated: N more bytes]`.
- Esc (or exit) while code runs discards the REPL worker and kills any active host command's process group; the result is the output so far plus `[stopped by user]`. Every complete call that never ran gets `[not run: turn stopped]`.
- SIGHUP, SIGTERM, and SIGINT stop the turn the same way (recording only, no transcript output) and exit.

## Tool reasoning

Experimental. With `[chat] tool_reasoning` on, each request and the transcript show the model its reasoning from earlier turns as tool calls instead of native reasoning. The session file is unchanged, so the setting can be switched either way at any time, including for resumed sessions. Deferred extension to the turn in progress: `SPEC-final-turn-tool-reasoning.md`.

- Converted: each assistant message before the last user message whose reasoning text (as displayed; see Terminal UI) is non-empty. Messages with only encrypted `reasoning_details` are left as-is. The turn in progress keeps its native reasoning.
- A converted message is sent as three messages: `{"role": "assistant", "content": "", "tool_calls": [REPL call, id reasoning-<message index>, code reasoning(<text as a JSON string literal>)]}`, `{"role": "tool", "tool_call_id": "reasoning-<message index>", "content": "(no output)"}`, then the message without `reasoning`, `reasoning_content`, and `reasoning_details`. Ids are stable across requests.
- These generated reasoning-history entries never run. Their `(no output)` result relies on `replib/reasoning.py` defining `reasoning(text)` as a no-op that prints nothing and returns `None`; `help()` lists it, and the model is meant to call it. Generating these entries does not make `reasoning()` a synthetic tool function.

## REPL

- Monty runs in a worker of the harness binary, managed by a single-worker pool. The first call of each turn checks out a fresh session and loads the library. State persists across that turn's calls; normal turn completion finishes the session and returns the worker. Cancellation discards it.
- Static type checking is disabled. Ordinary runtime exceptions preserve completed effects and surviving state. A worker crash or memory-limit failure discards state, reports that loss explicitly, and causes the next call to start a fresh session and reload the library without replaying failed code. Limits and recovery messages are specified in `SPEC-sandbox.md`.
- Filesystem operations use the repository mount and the current permission level: `none` denies reads and writes, `read-only` allows reads, and `read-write` allows both. Outside access is denied at every level. Permission changes apply to the next operation, including mid-call; path-containment rules are specified in `SPEC-sandbox.md`.
- `run(argv, cwd=None)` executes host commands outside the sandbox, independently of filesystem permissions. Command resolution, approvals, working-directory restrictions, environment filtering, and cancellation are specified in `SPEC-sandbox.md`.
- `replib/*.py` files execute in name order at session start under normal operation permissions. Public top-level functions are listed by source signature and docstring; names beginning with `_` are omitted. Load errors appear in the first call's output, failed files are omitted from the listing, and loading continues after ordinary errors.
- `help()` takes no arguments and prints the output limit, optional `repl-notes.md`, host functions, loaded public library functions, and allow-listed command names. `lib_source(name)` returns the current contents of `replib/<name>.py`. Starter-library behavior is specified in `SPEC-sandbox.md`.

## Terminal UI

- Required `[theme]` foreground colors: `system`, `user`, `assistant`, `reasoning`, `tool_call`, `tool_result`, `approval`, `warning`, `error`, `status`, `input`. Every value must be a `#RRGGBB` string; names, missing fields, and unknown keys are startup errors. Colors load at startup with no dimming or background override. Selection remains reverse video.
- Full screen on the alternate screen, with mouse capture. On exit the terminal returns to its previous contents; nothing is left behind.
- The transcript always matches what the model sees: every frame draws it from the system message, `messages`, and the turn in progress, so removed or changed messages disappear or change wherever they are. Fields other than content, reasoning, and tool calls are sent but not shown. With tool reasoning on, converted messages are drawn as sent: the generated reasoning-history call and its result, then the message without its reasoning.
  - System message (if configured): `system`, then the prompt, both in `theme.system`.
  - User message: `> text`, in `theme.user`.
  - Assistant: reasoning in `theme.reasoning`, a blank line, then content in `theme.assistant`. Reasoning is the first non-empty of `reasoning`, `reasoning_content`, and the `text` or `summary` of each `reasoning_details` entry (joined by blank lines). Converted reasoning history also uses `theme.reasoning`.
  - Tool call: `REPL` and code in `theme.tool_call`, any call notes, then the result in `theme.tool_result`. Calls appear once their name and id arrive; the code preview grows as arguments stream in, decoding complete JSON string characters and waiting for incomplete escapes. Malformed arguments are shown raw. Previewing changes neither execution nor interrupted-call persistence. Running REPL print output appears live, in full; host command output is captured until completion.
  - A blank line follows each message and each tool call.
  - Notes are shown but never saved and are lost on exit: failure notes use `theme.warning` under their call, and errors (`error: ...`) use `theme.error` where they happened. Call notes disappear when their call is removed; errors keep their place among the remaining messages.
- Word-wrapped by the app to the screen width and re-wrapped on resize. Rows break after whitespace; longer words break at the width. Tabs expand to 8-column stops; other control characters are not drawn.
- Scrolling: while at the bottom the view follows new output; scrolled up, it stays on the same text as output arrives or the width changes, until scrolled back to the bottom. Sending a message returns to the bottom.
- Selection: dragging selects transcript text (reverse video); dragging onto the top row or below the transcript scrolls one row per mouse event. Releasing copies the selection's source text (without wrap breaks) with OSC 52. The selection stays until the next click or key press. A failed copy shows the notice `couldn't copy: <error>`.
- Each frame is one synchronized update.
- Bottom region: a top rule in `theme.status`, a `> ` prompt and input text in `theme.input` (word-wrapped, growing up to half the screen height), and a status line, or the command completion list in its place (see Commands); the region grows to fit the list. Completion labels use `theme.status`, with the selected row reversed. A click there only clears the selection.
- Status line shows the model, reported token count when available, permission level, and `tool reasoning` when enabled. It uses `theme.status`, approval prompts/descriptions use `theme.approval`, and notices use `theme.warning`. Prompt and description behavior is specified in `SPEC-sandbox.md`.
- Bracketed paste is enabled. Pasted text is inserted as-is (CR and CRLF become LF) and never sends.

## Keys

| Key | Action |
|---|---|
| Enter | Send (ignored when blank); with the completion list open, run the highlighted command (or complete one that takes an argument; see Commands) |
| ^J | Insert newline |
| ^K | Delete to end of line; at end of line, join the next line |
| ^U | Delete to start of line; at start of line, join the previous line |
| Esc | Stop the turn; with the completion list open, only close it |
| Shift+Tab | Cycle filesystem permission: none → read-only → read-write → none; saved to configuration |
| Up / Down, Tab | With the completion list open: move the highlight (wrapping), complete to the highlighted command |
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
- `/rename <name>`: rename the session file to `sessions/<name>.json` (before the first save, only the name it will be created under). An invalid or existing name shows the notice `couldn't rename: <error>` and keeps the input; the current name is a no-op.
- `/tool-reasoning`: switch tool reasoning on or off and save it to `config.toml`. The transcript and the next request change at once.
- `/exit`, `/quit`: exit.
- Unknown command: notice `unknown command: <input>`; the input is kept.
- During a turn, the input stays editable and commands run. Enter on a message shows the notice `turn in progress (esc to stop)` and keeps the draft.

## Source

| File | Role |
|---|---|
| `src/main.rs` | Arguments, session listing, config and session loading |
| `src/app.rs` | Event loop, turn state machine, streaming display, saving, status line |
| `src/tui.rs` | Alternate screen, mouse capture, screen layout, synchronized updates, clipboard |
| `src/transcript.rs` | Transcript view: word wrap, scrolling, selection |
| `src/input.rs` | Textarea setup, key map, paste |
| `src/commands.rs` | Command table: names, aliases, descriptions, actions, arguments |
| `src/client.rs` | REPL tool definition, streaming request, SSE parsing and delta merging, approval-description request |
| `src/repl.rs` | Monty pool and per-turn session lifecycle, suspension handling, library loading, recovery |
| `src/worker.rs` | Monty worker protocol and memory limits |
| `src/host.rs` | Host functions, command execution and approvals, library metadata |
| `src/hostfs.rs` | Per-operation filesystem permissions and repository-mount dispatch |
| `src/failures.rs` | Failure-log persistence and occurrence counts |
| `src/config.rs` | Harness directory, configuration and endpoint credentials, permission and tool-reasoning persistence |
| `src/session.rs` | Message types, session file I/O, listing, renaming |
| `src/tool_reasoning.rs` | Tool reasoning conversion and the `reasoning()` approval exemption |
| `replib/reasoning.py` | `reasoning()` library function |
| `replib/glob.py` | Sorted relative-path globbing |
| `replib/search.py` | Regex file search with path, line, and text results |
