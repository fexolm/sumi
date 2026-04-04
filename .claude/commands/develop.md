Run the full multi-agent development pipeline for the following task:

$ARGUMENTS

## Pipeline

Execute phases sequentially. Pass full context between phases — each agent starts cold.

### Phase 1: Architecture

Spawn an Agent with `subagent_type: "kernel-architect"`.

Give it:
- The task: $ARGUMENTS
- Instruction to read CLAUDE.md and all relevant files first
- Request for a complete implementation plan with exact interfaces and ordered steps

Save the architect's output. Present it to the user and wait for approval before continuing.

### Phase 2: Implementation

Spawn an Agent with `subagent_type: "kernel-implementor"` and `mode: "auto"`.

Give it:
- The full architect's plan from Phase 1
- The list of files to read before making changes
- Instruction to implement exactly the plan, nothing more

After completion, capture the list of modified files.

### Phase 3: Review

Spawn an Agent with `subagent_type: "kernel-reviewer"`.

Give it:
- The task: $ARGUMENTS
- The architect's plan
- The list of modified files (tell it to read each one in full)
- The full diff: run `git diff` and include the output

The reviewer must check correctness, performance, safety, simplicity, edge cases, and integration.

Save the reviewer's verdict and issues list.

### Phase 4: Fix loop (if needed)

If the reviewer found CRITICAL or WARNING issues:

1. Spawn an Agent with `subagent_type: "kernel-implementor"` and `mode: "auto"`.
   Give it the exact list of issues from the reviewer and tell it to fix each one.

2. Spawn another `kernel-reviewer` agent. Give it the updated diff and the original issues list.
   Ask it to verify each fix was applied correctly.

Repeat up to 3 rounds. If unresolved after 3 rounds, stop and report to the user.

### Phase 5: Tests

Spawn an Agent with `subagent_type: "kernel-tester"` and `mode: "auto"`.

Give it:
- The task: $ARGUMENTS
- The list of modified files
- The reviewer's concerns and edge cases (so tests cover them)
- Instruction to run `cargo test` after writing tests

### Phase 6: Final verification

Run `cargo test` yourself. Report the results to the user with a summary:
- What was designed
- What was changed (files and a brief description)
- What tests were added
- Any issues found and resolved
