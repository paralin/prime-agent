/** Header OpenCode Zen and OpenCode Go require on each conversation request. */
export const OPENCODE_SESSION_HEADER = "x-opencode-session";

/** isOpenCodeProvider reports whether the catalog provider is OpenCode Zen or OpenCode Go. */
export function isOpenCodeProvider(provider: string | undefined): boolean {
	return provider === "opencode" || provider === "opencode-go";
}

/** getOpenCodeSessionHeaders returns the conversation header for OpenCode providers. */
export function getOpenCodeSessionHeaders(
	provider: string | undefined,
	sessionId: string | undefined,
): Record<string, string> {
	const id = sessionId?.trim();
	if (!id || !isOpenCodeProvider(provider)) {
		return {};
	}
	return { [OPENCODE_SESSION_HEADER]: id };
}
