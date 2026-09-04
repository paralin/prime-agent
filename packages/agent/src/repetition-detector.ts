/**
 * Stream-side detection of degenerate model repetition loops.
 */

const DEFAULT_REPETITION_LOOP_THRESHOLD = 5;
const REPETITION_MIN_BLOCK_CHARS = 4;
const REPETITION_MAX_PERIOD = 2048;
const REPETITION_WINDOW_CHARS = 4096;
const PARAGRAPH_MIN_CHARS = 60;
const PARAGRAPH_COPIES = 3;
const LINE_MIN_CHARS = 40;
const LINE_COPIES = 6;
const BLOCK_BUFFER_CHARS = 16_384;
const REPETITION_ALNUM_RE = /[\p{L}\p{N}]/u;
const REPETITION_WHITESPACE_RE = /\s/;

export const DEFAULT_REPETITION_THRESHOLD = DEFAULT_REPETITION_LOOP_THRESHOLD;

// Main-Lorentz: a run of r matches at period p is `floor((r-1)/p)+2`
// occurrences of the block. Require `threshold` occurrences for every period.
// A lower long-block copy count (3) treats two restated planning sections plus
// a trailing newline as a loop, which is ordinary thought, not degeneration.
function runNeeded(period: number, threshold: number): number {
	return (threshold - 2) * period + 1;
}

function normalizeLine(line: string): string {
	return line.trim().toLowerCase().replace(/\s+/g, " ");
}

/** RepetitionDetector watches streamed assistant text and thinking for periodic loops. */
export class RepetitionDetector {
	private readonly threshold: number;
	private readonly runs = new Array<number>(REPETITION_MAX_PERIOD + 1).fill(0);
	private window = "";
	private raw = "";

	constructor(threshold: number) {
		this.threshold = Math.max(2, threshold);
	}

	/** observeText returns true when streamed output has become a degenerate loop. */
	observeText(delta: string): boolean {
		if (this.observePeriodic(delta)) return true;
		return this.observeBlocks(delta);
	}

	private observePeriodic(delta: string): boolean {
		for (const raw of delta) {
			// Case and whitespace variants of the same block are the same loop.
			if (REPETITION_WHITESPACE_RE.test(raw) && this.window.endsWith(" ")) continue;
			const normalized = REPETITION_WHITESPACE_RE.test(raw) ? " " : raw.toLowerCase();
			this.window += normalized;
			if (this.window.length >= 2 * REPETITION_WINDOW_CHARS) {
				this.window = this.window.slice(-REPETITION_WINDOW_CHARS);
			}
			const index = this.window.length - 1;
			for (let period = REPETITION_MIN_BLOCK_CHARS; period <= REPETITION_MAX_PERIOD && period <= index; period++) {
				if (this.window[index] === this.window[index - period]) {
					const run = this.runs[period] + 1;
					this.runs[period] = run;
					if (
						run >= runNeeded(period, this.threshold) &&
						REPETITION_ALNUM_RE.test(this.window.slice(index + 1 - period))
					) {
						return true;
					}
				} else {
					this.runs[period] = 0;
				}
			}
		}
		return false;
	}

	private observeBlocks(delta: string): boolean {
		this.raw += delta;
		if (this.raw.length > 2 * BLOCK_BUFFER_CHARS) {
			this.raw = this.raw.slice(-BLOCK_BUFFER_CHARS);
		}
		return this.hasRepeatedParagraph() || this.hasRepeatedLine();
	}

	private hasRepeatedParagraph(): boolean {
		const parts = this.raw
			.split(/\n[ \t]*\n/)
			.map(normalizeLine)
			.filter((part) => part.length >= PARAGRAPH_MIN_CHARS);
		if (parts.length < PARAGRAPH_COPIES) return false;
		const last = parts[parts.length - 1]!;
		for (let i = 2; i <= PARAGRAPH_COPIES; i++) {
			if (parts[parts.length - i] !== last) return false;
		}
		return true;
	}

	private hasRepeatedLine(): boolean {
		const lines = this.raw
			.split("\n")
			.map(normalizeLine)
			.filter((line) => line.length >= LINE_MIN_CHARS);
		if (lines.length < LINE_COPIES) return false;
		const last = lines[lines.length - 1]!;
		for (let i = 2; i <= LINE_COPIES; i++) {
			if (lines[lines.length - i] !== last) return false;
		}
		return true;
	}
}
