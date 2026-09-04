/** Model-visible closeout turn sent immediately before scratch compaction. */
export const SCRATCH_HANDOFF_CLOSEOUT_GUIDANCE =
	"This turn only prepares the handoff file. Write a useful draft checkpoint before investigating uncertain details, then make targeted edits if needed. For an existing checkpoint, read it and update it in place. Record the newest user request, completed work, remaining TODOs, blockers, and the next concrete action. Separate facts by host, repository, and path. Preserve uncertainty explicitly instead of repeatedly reconstructing the entire conversation from memory. Earlier thinking is tentative, not evidence that an action happened. Use actual tool results and current state for disputed facts. Do not resume the underlying task during closeout. Finish after saving the file.";

export function renderScratchHandoffCloseoutMessage(displayPath: string, create = false): string {
	return create
		? `Stop working for now; please create a .org file brain-dump of your ongoing work to ${displayPath}, use org-todo structure including TODO subheadings, subheadings of subheadings, TODOs on nested subheadings, and so on. It should be detailed enough to hand off this work to a colleague.`
		: `Stop working for now and make any final edits to ${displayPath} such that you can hand it to a colleague to continue this work.`;
}
