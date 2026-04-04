Critically review the current uncommitted changes.

$ARGUMENTS

## Process

1. Run `git diff` and `git diff --cached` to gather all changes.

2. Spawn an Agent with `subagent_type: "kernel-reviewer"`.

   Give it:
   - The full diff output
   - Additional context from $ARGUMENTS if provided
   - The list of changed files (tell it to read each one in full, not just the diff)

3. Present the reviewer's findings to the user.

4. If the user confirms fixes are needed, spawn an Agent with `subagent_type: "kernel-implementor"` and `mode: "auto"` to apply them. Then re-run the reviewer to verify.
