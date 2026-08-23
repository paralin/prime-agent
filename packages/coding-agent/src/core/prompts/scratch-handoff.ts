/**
 * Scratch handoff prompts.
 *
 * A scratch checkpoint is a small org-mode state file the session maintains so
 * context maintenance can rebuild around it instead of paying for an
 * LLM-authored summary. One prompt block rides in the system prompt for the
 * whole session, one drives the closeout turn that refreshes the document, and
 * one is injected as the first message after a scratch-anchored compaction.
 */

/** Session-long continuity instructions appended to the system prompt. */
export function renderScratchHandoffInstructions(input: {
	displayPath: string;
	sessionId: string;
	exists: boolean;
	parentDisplayPath?: string;
}): string {
	const exists = input.exists
		? "Current contents already supplied as continuation state."
		: "File not created yet; create it only when closeout requests a checkpoint.";
	const parent = input.parentDisplayPath
		? `- Parent scratch: \`${input.parentDisplayPath}\`. Link when needed; NEVER write child state into parent.\n`
		: "";
	return `Scratch continuity:
- Path: \`${input.displayPath}\`; session: \`${input.sessionId}\`. ${exists}
- Continue as if no reset occurred. Scratch maintenance stays internal; NEVER report it as task progress or evidence unless user asks.
- Scratch = bounded current-state checkpoint, not session history or artifact dump.
- Keep exactly one root \`* TODO\` current-work subtree. Put future work under \`** TODO\`; completed history belongs in linked plans, logs, or artifacts.
- Root TODO MUST contain: Objective, minimal current Skill stack, Work completed, Files changed, Verification, Blockers or risks, Next action, Source refs.
- Skill stack: only dependencies needed by current TODO or next executable action, original relative order. Remove historical, completed-phase, one-shot, stale, superseded, duplicate skills. Empty allowed.
- Resume: select skills from current TODO + next action. Load only the first executable step's needed skill; apply normal matching for newly relevant skills. NEVER replay full stack or restart orientation.
- Update only on explicit closeout/handoff, except substantial completed work plus substantial remaining work under likely context pressure.
- Update existing root TODO in place; no duplicate status blocks. Link large evidence rather than copying it.
- Verification = current proof + residual risk, not command transcript.
- Keep \`#+TITLE\`, \`#+SESSION\`, \`#+PATH\`, optional \`#+PARENT_SCRATCH\` as root keywords; no wrapper heading.
${parent}- No update needed? Leave unchanged. NEVER mention scratch state/path in final response unless asked.`;
}

/** Closeout turn: pencils-down checkpoint write before a scratch compaction. */
export function renderScratchHandoffCloseoutMessage(displayPath: string, create = false): string {
	const createOrUpdate = create ? "create" : "update";
	const shape = create
		? "Create one bounded Org checkpoint with root metadata and exactly one active `* TODO` subtree."
		: "Keep exactly one active `* TODO` subtree; revise the document at that same path without renaming it.";
	return `Context maintenance threshold reached. PENCILS DOWN.
Maintenance only: before more task work, run exactly one \`ipython {code}\` cell. Use \`from pathlib import Path\`, bind \`scratch = Path("${displayPath}")\`, make \`scratch.parent\` when needed, and call \`scratch.write_text(..., encoding="utf-8")\` to ${createOrUpdate} that exact path.
${shape}
Checkpoint MUST capture current objective; only immediately required skills; completed work; touched files; current proof; blockers/risks; executable next action; continuation source refs.
Remove completed-history subtrees and copied plans, queues, logs, traces, or large evidence; preserve them through links.
After successful \`ipython\`, NEVER reread or verify scratch. END TURN immediately: no task work, other cells, user-facing status, or path mention. Runtime observes the changed document; successor resumes.
`;
}

export interface ScratchHandoffResumeInput {
	displayPath: string;
	scratchText: string;
	exists: boolean;
	parentDisplayPath?: string;
	recentContextText?: string;
	scratchTruncated?: boolean;
}

/** First model-visible message after a scratch-anchored compaction. */
export function renderScratchHandoffResumeMessage(input: ScratchHandoffResumeInput): string {
	if (!input.exists) {
		return `No scratch checkpoint exists yet. Continue current task from live conversation. Do not read \`${input.displayPath}\`; create it only when explicit closeout requests a checkpoint.

<recent-session-context>
Session context newer than checkpoint:

${(input.recentContextText ?? "").trim()}
</recent-session-context>`;
	}
	const truncated = input.scratchTruncated
		? `Only checkpoint beginning is injected. Use one \`ipython {code}\` cell with \`pathlib.Path\` to read \`${input.displayPath}\` only if active TODO, next action, or a required referenced detail is missing below.`
		: `Do not reread \`${input.displayPath}\`; full checkpoint is supplied.`;
	const parent = input.parentDisplayPath ? `Parent scratch: ${input.parentDisplayPath}\n` : "";
	const delta = input.recentContextText?.trim()
		? `\n<recent-session-context>\nSession context newer than checkpoint:\n\n${input.recentContextText.trim()}\n</recent-session-context>`
		: "";
	return `Resume existing work from supplied scratch checkpoint.
Choose skills from active TODO + next action. Load only the first executable step's needed skill; skip stale, historical, duplicate entries; apply normal matching for missing skills. NEVER replay full stack or restart orientation.
Injected checkpoint and recent delta are continuation state. Do not summarize, reconstruct completed work, or rerun stable checks unless newer evidence invalidates them.
${truncated}
Batch first step's live checks and execute in same turn. Do not repeat startup repair.

${parent}<scratch-handoff-context>
Path: ${input.displayPath}

${input.scratchText}
</scratch-handoff-context>${delta}`;
}
