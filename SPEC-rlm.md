# Recursive language model agents

Contract for the root agent, branch agents, the library, and the store. Base contract: `SPEC.md`, which this contract supersedes where they differ. Implementation plan: `PLAN-rlm.md`.

## Agents

- One kind of agent. The root is depth 0. A branch spawned by an agent at depth `d` is at depth `d + 1`. Tools, mounts, and host functions are fixed by depth. Every agent receives the `[chat] prompt` system message; when `[rlm] subagent_prompt` is set, subagents receive it instead.
- The root is a pure delegator. Its tools are `rlm` and `FYI`. It has no REPL, no repository access, and no store access. `FYI` takes no arguments and returns the date, the prompt-token count of the latest request, and the model. Each root turn opens with a synthetic `FYI` call.
- Each branch turn opens with synthetic `FYI()`, `help()`, and `store_summary()` REPL calls, plus `library_summary()` where `/library` is mounted.
- `rlm` takes two required strings: `task`, which becomes the branch's user message verbatim, and `name`, a short label for the branch shown to the user.
- A branch is one-shot: it receives a task, runs until a response has no tool calls, and ends. That response's content is the `rlm` result, subject to the parent's `output_limit` like any tool result. No agent resumes a branch.
- The `rlm` calls in one response run one at a time, in call order, in the ordered tool queue with the response's other calls. Results are saved as tool messages in call order.
- Anything a branch returns is opaque text to its parent, including references into the store.

## Bounds

- Maximum depth is configured and enforced by the harness: `rlm` is not offered to agents at the maximum depth. Every `rlm` call in a response runs, in call order.

## Files

- A branch is a session file in the session format. Its ID uses the encoded call-ID format.
- A tree is one directory, `sessions/<name>/`. `library/` holds the root transcript as `root.json` and depth 1 branches as `<id>.json`. `deep/` holds depth 2 and deeper as `<id>.json`. `/rename` renames the directory. Session listing shows directory names.
- Every session records under `_harness_local` its repository root, the canonical launch directory of the process that created it, and its depth, 0 for the root. Every branch also records its parent: `root` at depth 1, the parent's call ID deeper. These fields are immutable.

## Library

- The library is this tree's depth 0 and 1 transcripts, `sessions/<name>/library/`.
- It is a read-only mount into the REPL sessions of agents at depth 2 and deeper, at the virtual path `/library`, and the direct tools' `root = "library"`. Agents at depth 0 and 1 have no mount of any session file. `deep/` is never mounted. Trees never read each other's files.

## Store

- The store is a configured directory, `[rlm.store] path`.
- It is a read-write mount into agents at depth 1 and deeper, at the REPL virtual path `/store` and the direct tools' `root = "store"`, independent of the repository permission level. The root has no store access.
- A reference is a store path in text. Pass by reference is naming a path in a task or an answer.
- Writes are ordinary filesystem writes. The harness holds no locks and mediates nothing.

## Data kinds

- REPL variables are memory: local to one agent, alive for one turn, never addressable by another agent.
- Transcripts are durable. A branch transcript is append-only while its agent runs and fixed once it ends. The root transcript may also be rewritten as `SPEC.md` describes (turn-start `FYI` cleanup, hidden turns, token cache), never while a branch runs. Every save replaces the file atomically, so a reader sees a complete version, never a partial one.
- The store and the repository are durable and shared.
- Memory becomes durable only by an explicit write to the store or the repository. Durable data enters memory only by an explicit read.

## Human control

- Approvals from any branch arrive at the one approval prompt.
- Esc stops the whole tree; the deepest agent's partial response followed by `[stopped by user]` reaches the root as the `rlm` result at each level.
- The filesystem permission level applies to every branch.
- Every branch file is inspectable as a session.
- The transcript, context overlay, and status line follow the deepest active agent and return to its parent when it ends. The status line names the agent on screen: the session name for the root, its `name` for a branch. The context overlay's header names every agent on the stack the same way, root first, as `[Context][<root>]…[<on-screen agent>]`, and when the width cap binds, the oldest ancestors collapse into one `[…]` segment, the on-screen agent's name shortening with an ellipsis only if it alone still does not fit.

## Removed

- Tool reasoning. The `reasoning()` approval exemption remains.
- File injection.
- Pre-RLM session files: they are neither listed nor resumable.

## Deferred

- Concurrent branch execution, with a per-response fan-out bound.
- A tabbed context overlay for switching between agents' context views.
- Messaging between agents through the store.
- Opening a branch file with `--resume`.
