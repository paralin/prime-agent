/**
 * Global, environment-scoped startup notices (tmux setup).
 *
 * These are not tied to any single conversation, so they are surfaced on the agents
 * view rather than appended to a session's chat stream. The check and styled
 * formatter live here so both the agents view and the interactive fallback render
 * identical wording.
 */

import { spawn } from "node:child_process";
import { theme } from "../interactive/theme/theme.js";

export interface StartupNotices {
	/** tmux keyboard setup warning, if the current tmux config is suboptimal. */
	tmuxWarning?: string;
}

/** Run every startup check and collect the results. */
export async function gatherStartupNotices(): Promise<StartupNotices> {
	const tmuxWarning = await checkTmuxKeyboardSetup();
	return { tmuxWarning };
}

export async function checkTmuxKeyboardSetup(): Promise<string | undefined> {
	if (!process.env.TMUX) return undefined;

	const runTmuxShow = (option: string): Promise<string | undefined> => {
		return new Promise((resolve) => {
			const proc = spawn("tmux", ["show", "-gv", option], {
				stdio: ["ignore", "pipe", "ignore"],
			});
			let stdout = "";
			const timer = setTimeout(() => {
				proc.kill();
				resolve(undefined);
			}, 2000);

			proc.stdout?.on("data", (data) => {
				stdout += data.toString();
			});
			proc.on("error", () => {
				clearTimeout(timer);
				resolve(undefined);
			});
			proc.on("close", (code) => {
				clearTimeout(timer);
				resolve(code === 0 ? stdout.trim() : undefined);
			});
		});
	};

	const [extendedKeys, extendedKeysFormat] = await Promise.all([
		runTmuxShow("extended-keys"),
		runTmuxShow("extended-keys-format"),
	]);

	// If we couldn't query tmux (timeout, sandbox, etc.), don't warn
	if (extendedKeys === undefined) return undefined;

	if (extendedKeys !== "on" && extendedKeys !== "always") {
		return "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux.";
	}

	if (extendedKeysFormat === "xterm") {
		return "tmux extended-keys-format is xterm. Pi works best with csi-u. Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux.";
	}

	return undefined;
}

export function formatTmuxWarningNotice(message: string): string {
	return theme.fg("warning", `⚠ ${message}`);
}
