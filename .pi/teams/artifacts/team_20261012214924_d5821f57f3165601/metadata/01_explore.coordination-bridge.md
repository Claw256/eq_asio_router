# Crew Coordination Channel
Mailbox target for this task: 01_explore
Use the run mailbox contract for coordination with the leader/orchestrator:
- If blocked or uncertain, report the blocker in your final result and, when mailbox tools/API are available, send an inbox/outbox message addressed to the leader.
- Never guess implementation details that materially affect decisions. If the `ask` tool is available and you need a clarification, a decision, or a missing requirement before you can proceed safely, call `ask` and wait — a parked question is cheaper than a wrong build.
- Ask the leader before editing when scope is ambiguous, requirements conflict, destructive action is needed, or you discover likely overlap with another task.
- Before making non-trivial edits, state intended changed files in your notes/result; if another worker may touch the same file/symbol, pause and request sequencing/ownership guidance.
- Do not resolve cross-worker conflicts silently. Escalate via mailbox/result with: file/symbol, conflicting task if known, proposed owner, and safest next step.
- If nudged, answer with current status, blocker, or smallest next step.
- Treat inherited/dependency context as reference-only; do not continue the parent conversation directly.
- Completion handoff must use the structured 6-section template (Goal / Constraints & Preferences / Progress / Key Decisions / Next Steps / Critical Context): mark DONE/FAILED and split Done·In Progress·Blocked under Progress, list changed/read files and verification evidence under Critical Context, and surface remaining risks as Next Steps.
