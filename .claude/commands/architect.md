Design the architecture for the following task:

$ARGUMENTS

## Process

Spawn an Agent with `subagent_type: "kernel-architect"`.

Give it:
- The task: $ARGUMENTS
- Instruction to read CLAUDE.md and all relevant files first
- Request for a complete plan: alternatives considered, chosen approach with justification, exact interfaces, implementation order, edge cases, test plan

Present the architect's plan to the user. Do not proceed to implementation unless explicitly asked.
