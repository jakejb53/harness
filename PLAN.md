# Implementation Plan

Working log. Behavior contract: `SPEC.md`.

## Done

1. **Terminal layer spike.** Raw transcript in scrollback plus an app-managed bottom input region; resize via the parked hidden cursor; bracketed paste. Scrollback and cursor parts superseded by 15.
2. **Chat app.** `config.toml` and `.env`, session files with atomic writes, SSE streaming client, `/exit` and `/quit`, Esc to stop, `--resume <PATH>`.
3. **Review fixes.** Status line erased at screen bottom; partial reply lost on error paths; raw mode left on failed startup; stale pending-line row after scroll; per-delta cursor-query lag (deltas coalesced, input poll 20 ms); resize ordering (`biased` select); temp file fsync. Scrollback-specific fixes superseded by 15.
4. **Flicker fix.** Synchronized updates per frame.
5. **Reasoning.** Streamed as `delta.reasoning`, displayed dim, saved as `reasoning`, sent back.
6. **Input prompt.** Bright white `> ` in the input box.
7. **Context tokens.** `stream_options.include_usage`; status line shows `total_tokens`.
8. **Tool calling.** Single `REPL` tool; per-turn local Python driver (`src/repl_driver.py`) with `replib/` registry and `help()`; `approval_mode` allow/ask/auto (Shift+Tab, persisted via `toml_edit`); LLM classifier for `auto`; `output_limit` (default 100 KB).
9. **Tool-calling hardening (review).** Results recorded before terminal I/O; driver `setsid()` (no controlling tty); request-reader thread; driver survives closed stdout; config save keeps decor, handles inline tables, atomic; `finish_reason: "length"` drops cut-off calls; SIGHUP/SIGTERM/SIGINT cleanup; resume answers unanswered calls; `.env` read privately.
10. **Situational awareness.** Each turn opens with only a synthetic `FYI()` REPL call, run in the REPL like any other (honesty rule: synthetic calls must work when made explicitly). `FYI()` is implemented in `src/host.rs`: date, latest `prompt_tokens` (sent with every code request), model. `help()` runs only on demand. The focused turn-start test and `cargo check` passed after removing automatic `help()`.
11. **Approval exemption.** Code made only of `FYI()` and `help()` statements never needs approval. The governing contract in `SPEC.md` covers all present and future synthetic tool calls, whether app-generated or explicitly called by the model, in every approval mode and permission level. `reasoning()` is separately exempt and is not synthetic. Neither exemption covers unrelated code bundled with the calls, including code evaluated in arguments.
12. **Instant resume replay.** The session replays as one batched write, so it appears at once instead of visibly scrolling. Superseded by 15.
13. **Synthetic-call cleanup.** Each turn removes all earlier `FYI()`/`help()`-only calls (and results, and emptied assistant messages) from the session before adding its `FYI()` call. First case of the app removing messages.
14. **Chat system message.** Optional `[chat] prompt`, prepended to each chat request at send time and never saved, so the current config applies to resumed sessions.
15. **App-managed scrollback.** Alternate screen with mouse capture; the transcript is drawn each frame from the system message, messages, the turn in progress, and display-only notes, so it always matches the model's context. App word wrap, scrolling (follow at bottom, PageUp/PageDown, wheel), drag selection with OSC 52 copy on release.
16. **Endpoint-neutral replies.** Each streamed `delta` is merged whole by the OpenAI SDK's `accumulate_delta` rule, and every field is saved and sent back unchanged, so any endpoint's reasoning format (`reasoning`, `reasoning_content`, `reasoning_details`) round-trips. Only display picks known reasoning fields.
17. **Tool reasoning (experimental).** `[chat] tool_reasoning` / `/tool-reasoning`: earlier turns' reasoning is sent and drawn as generated reasoning-history entries containing `reasoning("…")` calls with `(no output)` results, built per request from the unchanged session. First library function, `replib/reasoning.py`: a no-op the model is meant to call, listed by `help()`, separately exempt from approval when called alone on a plain string literal; it is not a synthetic tool function.
18. **Command completion.** Typing `/` lists matching commands (name, description) below the input in place of the status line; Up/Down, Tab, Enter, Esc. One command table (`src/commands.rs`) drives both completion and running.
19. **Harness directory.** `config.toml`, `.env`, `sessions/`, and `replib/` live in `~/.harness`, so the app runs in any repo. Sessions are flat and named by file name: `/rename <name>` (first command with an argument), `--resume` lists names, `--resume <NAME>` resumes.
20. **Sandbox.** Monty replaces CPython and the classifier, with repository permissions, brokered commands, library functions, failure logging, and worker recovery. Static type checking is disabled. Essential checks and the user-reported smoke test passed; skipped checks are recorded in `PLAN-sandbox.md`. Library-authoring features remain deferred. Contract: `SPEC-sandbox.md`.

Decided against:
- Audit-hook gating as the first permission layer (classifier chosen instead).
- Saving the system message in the session (config edits wouldn't reach resumed sessions).

## Next

1. **AGENTS.md.** Optional `[chat] agents_file` (absolute path). A synthetic `REPL` call using the shared encoded call-ID format reads the file and prints it as a JSON object; it lives in the first turn's synthetic assistant message, ahead of `FYI()`. Every turn, before cleanup: run it; insert it after the first user message if missing; replace code and result if changed; remove it if the setting is removed. Result truncated at `output_limit` like any call.
2. **Context management.** Proactive pruning so compaction is never needed; replaces the interim `output_limit`.
3. **`replib/` functions.** File helpers (`read`/`write`/`edit`), subagents (as host functions), knowledge-base search.
4. **Final-turn tool reasoning** (to revisit): convert the turn in progress's reasoning too. Design: `SPEC-final-turn-tool-reasoning.md`.
5. **Harness as teacher.** Turn the failure log (`SPEC-sandbox.md`) into proposed edits to the persistent guidance, for user approval.

## Open

- Two instances started in the same second, in any repos, share a session file name and overwrite each other.
- `serde_json` lacks `preserve_order`, so kept fields are saved and sent back with object keys sorted (values unchanged). Fix: enable the feature in `Cargo.toml`.

## Reference

- Run: `harness` (`target/debug/` on `PATH`) from the target repo, with `~/.harness` symlinked to this repo during development; `harness --resume [NAME]`.
- Python audit hooks (PEP 578) report `open`, `os.remove`/`rename`, `subprocess.Popen`, `os.system`, `socket.connect`, `ctypes.*`, and `import` events with real arguments; not a security boundary.
