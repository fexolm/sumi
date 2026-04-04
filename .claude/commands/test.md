Write and run tests for the current changes or specified area.

$ARGUMENTS

## Process

1. Identify what to test:
   - If $ARGUMENTS specifies a file or module, test that.
   - Otherwise run `git diff` to find changed files.

2. Spawn an Agent with `subagent_type: "kernel-tester"` and `mode: "auto"`.

   Give it:
   - The target files or modules
   - The diff if testing recent changes
   - Any reviewer concerns to cover (if known)
   - Instruction to run `cargo test` after writing tests and fix any failures

3. Report the results: tests added, what they cover, any bugs found.
