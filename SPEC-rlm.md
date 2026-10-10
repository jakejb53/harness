# Recursive language model agents

Contract for the root agent, branch agents, the library, and the store. Base contract: `SPEC.md`. Working log: `PLAN.md`.

## Agents

- One kind of agent. The root is depth 0. A branch spawned by an agent at depth `d` is at depth `d + 1`. Tools, prompts, and host functions are configured per depth.
- The root is a pure delegator. Its only tool is `rlm`. It has no REPL, no repository access, no store access, and no turn-start synthetic call. Each `rlm` result carries FYI metadata: the date, the prompt-token count of the latest request, and the model.
- A branch is one-shot: it receives a task, runs until a response has no tool calls, and ends. That response's content is the `rlm` result. No agent resumes a branch.
- Every branch is told its own ID, its parent's ID, and its siblings' IDs.
- The `rlm` calls in one response start at once and run concurrently. Other calls in that response run in the ordered tool queue. Results are saved as tool messages in completion order.
- Anything a branch returns is opaque text to its parent, including references into the store.

## Bounds

- Maximum depth and maximum fan-out per response are configured and enforced by the harness. A call beyond either limit receives an error result and does not run.

## Files

- A branch is a session file in the session format. Its ID uses the encoded call-ID format.
- A tree is one directory, `sessions/<name>/`. `library/` holds the root transcript as `root.json` and depth 1 branches as `<id>.json`. `deep/` holds depth 2 and deeper as `<id>.json`. `/rename` renames the directory. Session listing shows directory names.
- Every session records its repository root under `_harness_local`. Every branch records its parent ID and depth under `_harness_local`. These fields are immutable.

## Library

- The library is this tree's depth 0 and 1 transcripts, `sessions/<name>/library/`.
- It is a read-only mount into the REPL sessions of agents at depth 2 and deeper. Agents at depth 0 and 1 have no mount of any session file. `deep/` is never mounted. Sessions never read each other's files.

## Store

- The store is a SilverBullet space reached over its HTTP API. Its base URL and optional credential variable are configured like an endpoint.
- Agents at depth 1 and deeper have host functions for reading, writing, and listing pages. The root has none.
- A reference is a page name in text. Pass by reference is naming a page in a task or an answer.
- For agents at depth 1 and deeper, when a tool result is truncated at `output_limit`, the full result is written to a store page and the truncation notice names it. The root's `rlm` results truncate with the plain notice. A branch's returned content is subject to its parent's `output_limit` like any tool result.
- Writes are last-write-wins. The harness holds no locks.

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

## Deferred

- The `rlm` invocation form and argument set.
- Configuration shape for per-depth tools, prompts, bounds, and the store endpoint.
- Messaging between agents through the store.
- Live display of running branches.
- Opening a branch file with `--resume`.
- Expiring write locks on store pages, if clobbering appears.
- Virtual mount path of the library.
- Exact store host-function set beyond read, write, and list.
