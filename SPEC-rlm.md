# Recursive language model agents

Contract for the root agent, branch agents, the library, and the store. Base contract: `SPEC.md`. Implementation plan: `PLAN-rlm.md`.

## Agents

- One kind of agent. The root is depth 0. A branch spawned by an agent at depth `d` is at depth `d + 1`. Tools, mounts, and host functions are fixed by depth. Every agent receives the `[chat] prompt` system message.
- The root is a pure delegator. Its tools are `rlm` and `FYI`. It has no REPL, no repository access, and no store access. `FYI` takes no arguments and returns the date, the prompt-token count of the latest request, and the model. Each root turn opens with a synthetic `FYI` call.
- `rlm` takes one required string, `task`, which becomes the branch's user message verbatim.
- A branch is one-shot: it receives a task, runs until a response has no tool calls, and ends. That response's content is the `rlm` result, subject to the parent's `output_limit` like any tool result. No agent resumes a branch.
- The `rlm` calls in one response run one at a time, in call order, in the ordered tool queue with the response's other calls. Results are saved as tool messages in call order.
- Anything a branch returns is opaque text to its parent, including references into the store.

## Bounds

- Maximum depth and maximum fan-out per response are configured and enforced by the harness. `rlm` is not offered to agents at the maximum depth. In one response, `rlm` calls beyond the fan-out limit, in call order, receive an error result and do not run.

## Files

- A branch is a session file in the session format. Its ID uses the encoded call-ID format.
- A tree is one directory, `sessions/<name>/`. `library/` holds the root transcript as `root.json` and depth 1 branches as `<id>.json`. `deep/` holds depth 2 and deeper as `<id>.json`. `/rename` renames the directory. Session listing shows directory names.
- Every session records its repository root under `_harness_local`. Every branch records its parent ID and depth under `_harness_local`. These fields are immutable.

## Library

- The library is this tree's depth 0 and 1 transcripts, `sessions/<name>/library/`.
- It is a read-only mount into the REPL sessions of agents at depth 2 and deeper, at the virtual path `/library`, and the direct tools' `root = "library"`. Agents at depth 0 and 1 have no mount of any session file. `deep/` is never mounted. Sessions never read each other's files.

## Store

- The store is a configured directory, `[rlm.store] path`.
- It is a read-write mount into agents at depth 1 and deeper, at the REPL virtual path `/store` and the direct tools' `root = "store"`, independent of the repository permission level. The root has no store access.
- A reference is a store path in text. Pass by reference is naming a path in a task or an answer.
- Writes are ordinary filesystem writes. The harness holds no locks and mediates nothing.

## Data kinds

- REPL variables are memory: local to one agent, alive for one turn, never addressable by another agent.
- Transcripts are durable. A transcript is append-only while its agent runs and fixed once it ends. Every save replaces the file atomically, so a reader sees a complete version, never a partial one.
- The store and the repository are durable and shared.
- Memory becomes durable only by an explicit write to the store or the repository. Durable data enters memory only by an explicit read.

## Human control

- Approvals from any branch arrive at the one approval prompt.
- Esc stops the whole tree.
- The filesystem permission level applies to every branch.
- Every branch file is inspectable as a session.
- The transcript, context overlay, and status line follow the deepest active agent and return to its parent when it ends. The status line names the agent on screen.

## Deferred

- Concurrent branch execution.
- Messaging between agents through the store.
- Opening a branch file with `--resume`.
