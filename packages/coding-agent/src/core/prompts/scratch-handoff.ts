/** Model-visible closeout turn sent immediately before scratch compaction. */
export function renderScratchHandoffCloseoutMessage(displayPath: string, create = false): string {
	return create
		? `Stop working for now; please create a .org file brain-dump of your ongoing work to ${displayPath}, use org-todo structure including TODO subheadings, subheadings of subheadings, TODOs on nested subheadings, and so on. It should be detailed enough to hand off this work to a colleague.`
		: `Stop working for now and make any final edits to ${displayPath} such that you can hand it to a colleague to continue this work.`;
}
