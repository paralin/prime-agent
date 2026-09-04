import { deflateSync } from "node:zlib";
import type { ImageContent } from "@earendil-works/pi-ai";
import type { SessionEntry } from "./session-manager.js";

const FRAME_WIDTH = 1280;
const FRAME_HEIGHT = 1280;
const CELL_WIDTH = 10;
const CELL_HEIGHT = 16;
const GLYPH_WIDTH = 5;
const GLYPH_HEIGHT = 8;
const FRAME_COLUMNS = FRAME_WIDTH / CELL_WIDTH;
const FRAME_ROWS = FRAME_HEIGHT / CELL_HEIGHT;
const FRAME_CAPACITY = FRAME_COLUMNS * FRAME_ROWS;
const MAX_FRAMES = 8;
const MAX_RENDERED_CELLS = FRAME_CAPACITY * MAX_FRAMES;
const TOOL_TEXT_MAX_CHARS = 8_000;
const TOOL_ARGUMENTS_MAX_CHARS = 4_000;
const HEAD_SHARE = 0.25;

// ASCII glyph rows from the public-domain X.org 5x8 BDF font used by
// oh-my-pi's snapcompact renderer. Each code point from space through tilde
// occupies eight bytes, with its five pixels in the high bits.
const FONT_ROWS = Buffer.from(
	"AAAAAAAAAAAAICAgIAAgAABQUFAAAAAAUFD4UPhQUAAgcKBwKHAgAABAUCBQEAAAQKCgQKCgUAAAICAgAAAAAAAgQEBAQCAAAEAgICAgQAAAAJBg8GCQAAAAICD4ICAAAAAAAAAwIEAAAAAA8AAAAAAAAAAAIHAgABAQIECAgAAAIFBQUFAgAAAgYCAgIHAAAGCQEGCA8AAA8CBgEJBgAAAgYKDwICAAAPCA4BCQYAAAYIDgkJBgAADwECAgQEAAAGCQYJCQYAAAYJCQcBBgAAAAYGAAYGAAAAAwMAAwIEAAECBAQCAQAAAAAPAA8AAAAEAgEBAgQAAAIFAQIAAgADBImKiokEAwAGCQkPCQkAAA4JDgkJDgAABgkICAkGAAAOCQkJCQ4AAA8IDggIDwAADwgOCAgIAAAGCQgLCQYAAAkJDwkJCQAABwICAgIHAAAHAgICCgQAAAkKDAoKCQAACAgICAgPAAAJDw8JCQkAAAkNDwsLCQAABgkJCQkGAAAOCQkOCAgAAAYJCQ0LBgEADgkJDgkJAAAGCQQCCQYAAAcCAgICAgAACQkJCQkGAAAJCQkJBgYAAAkJCQ8PCQAACQkGBgkJAAAIiIUCAgIAAA8BAgQIDwAABwQEBAQHAAAICAQCAQEAAAcBAQEBBwAAAgUAAAAAAAAAAAAAAAAPAAQCAAAAAAAAAAAHCQkHAAAICA4JCQ4AAAAAAwQEAwAAAQEHCQkHAAAAAAYLDAYAAAIFBA4EBAAAAAAGCQcBBgAICA4JCQkAAAIABgICBwAAAQABAQEFAgAICAkOCQkAAAYCAgICBwAAAAANCoqKgAAAAA4JCQkAAAAABgkJBgAAAAAOCQ4ICAAAAAcJBwEBAAAACg0ICAAAAAADBgEGAAAEBA4EBQIAAAAACQkJBwAAAAAFBQUCAAAAAAiKioUAAAAACQYGCQAAAAAJCQcJBgAAAA8CBA8AAwQCDAIEAwAAAgICAgICAAwCBAMEAgwAAAUKAAAAAAAA==",
	"base64",
);

export interface HistorySnapshot {
	text: string;
	images: ImageContent[];
	messageCount: number;
	truncated: boolean;
}

export interface HistoryTextLayout {
	pages: string[];
	truncated: boolean;
}

/** Serialize message-bearing session entries into renderer-neutral chronological source. */
export function serializeSessionHistory(entries: readonly SessionEntry[]): string {
	const parts: string[] = [];
	for (const entry of entries) {
		if (entry.type === "message") {
			const message = entry.message;
			if (message.role === "user") {
				const text = contentText(message.content);
				if (text) parts.push(`USER\n${text}`);
			} else if (message.role === "assistant") {
				const blocks: string[] = [];
				for (const content of message.content) {
					if (content.type === "text" && content.text) blocks.push(content.text);
					else if (content.type === "thinking" && content.thinking) blocks.push(`THINK\n${content.thinking}`);
					else if (content.type === "toolCall") {
						const args = truncate(JSON.stringify(content.arguments), TOOL_ARGUMENTS_MAX_CHARS);
						blocks.push(`CALL ${content.name}\n${args}`);
					}
				}
				if (blocks.length > 0) parts.push(`ASSISTANT\n${blocks.join("\n")}`);
			} else if (message.role === "toolResult") {
				const text = truncate(contentText(message.content), TOOL_TEXT_MAX_CHARS);
				if (text) parts.push(`RESULT ${message.toolName}${message.isError ? " ERROR" : ""}\n${text}`);
			}
		} else if (entry.type === "custom_message") {
			const text = contentText(entry.content);
			if (text) parts.push(`CONTEXT ${entry.customType}\n${truncate(text, TOOL_TEXT_MAX_CHARS)}`);
		} else if (entry.type === "compaction" || entry.type === "branch_summary") {
			if (entry.summary) parts.push(`SUMMARY\n${truncate(entry.summary, TOOL_TEXT_MAX_CHARS)}`);
		}
	}
	return normalize(parts.join("\n\n"));
}

/** Render chronological source into deterministic bounded PNG pages. */
export function buildHistorySnapshot(text: string, messageCount: number): HistorySnapshot {
	const normalized = normalize(text);
	if (!normalized) return { text: "", images: [], messageCount, truncated: false };
	const layout = layoutHistoryText(normalized);
	return {
		// Keep the source intact so later generations do not compound image loss.
		text: normalized,
		images: renderFrames(layout.pages),
		messageCount,
		truncated: layout.truncated,
	};
}

/** Re-render a new generation from prior chronological source plus new entries. */
export function buildSessionHistorySnapshot(input: {
	entries: readonly SessionEntry[];
	previous?: Pick<HistorySnapshot, "text" | "messageCount">;
}): HistorySnapshot {
	const currentText = serializeSessionHistory(input.entries);
	const text = [input.previous?.text, currentText].filter((part): part is string => Boolean(part)).join("\n\n");
	const currentMessageCount = input.entries.filter(
		(entry) => entry.type === "message" || entry.type === "custom_message",
	).length;
	return buildHistorySnapshot(text, (input.previous?.messageCount ?? 0) + currentMessageCount);
}

function contentText(content: string | readonly { type: string; text?: string }[]): string {
	if (typeof content === "string") return content;
	return content.flatMap((part) => (part.type === "text" && part.text ? [part.text] : [])).join("\n");
}

function truncate(text: string, maxChars: number): string {
	if (text.length <= maxChars) return text;
	const head = Math.floor(maxChars * HEAD_SHARE);
	const tail = maxChars - head;
	return `${text.slice(0, head)}\n[${text.length - maxChars} characters elided]\n${text.slice(-tail)}`;
}

function normalize(text: string): string {
	return text
		.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
		.replace(/\r\n?/g, "\n")
		.replace(/[\t\f\v ]+/g, " ")
		.replace(/\n{3,}/g, "\n\n")
		.trim();
}

/** Bound history by rendered cells while preserving its chronological tail. */
export function layoutHistoryText(text: string): HistoryTextLayout {
	const cells = layoutCells(text);
	if (cells.length <= MAX_RENDERED_CELLS) return { pages: paginateCells(cells), truncated: false };

	const marker = layoutCells(`[${cells.length - MAX_RENDERED_CELLS} rendered cells elided]`);
	const available = MAX_RENDERED_CELLS - marker.length;
	const head = Math.floor(available * HEAD_SHARE);
	const bounded = [...cells.slice(0, head), ...marker, ...cells.slice(-(available - head))];
	return { pages: paginateCells(bounded), truncated: true };
}

function renderFrames(pages: readonly string[]): ImageContent[] {
	return pages.map((page) => ({
		type: "image",
		mimeType: "image/png",
		data: encodePng(renderPage(page), FRAME_WIDTH, FRAME_HEIGHT).toString("base64"),
	}));
}

function layoutCells(text: string): string[] {
	const cells: string[] = [];
	let column = 0;
	for (const raw of text) {
		const char = printable(raw);
		if (char === "\n") {
			if (column === 0) cells.push(...Array<string>(FRAME_COLUMNS).fill(" "));
			else
				while (column < FRAME_COLUMNS) {
					cells.push(" ");
					column++;
				}
			column = 0;
			continue;
		}
		if (column === FRAME_COLUMNS) column = 0;
		cells.push(char);
		column++;
	}
	return cells;
}

function paginateCells(cells: readonly string[]): string[] {
	const pages: string[] = [];
	for (let offset = 0; offset < cells.length; offset += FRAME_CAPACITY) {
		pages.push(cells.slice(offset, offset + FRAME_CAPACITY).join(""));
	}
	return pages;
}

function printable(char: string): string {
	if (char === "\n") return char;
	const code = char.codePointAt(0) ?? 63;
	return code >= 32 && code <= 126 ? char : "?";
}

function renderPage(text: string): Buffer {
	const pixels = Buffer.alloc(FRAME_WIDTH * FRAME_HEIGHT, 255);
	for (let index = 0; index < text.length && index < FRAME_CAPACITY; index++) {
		const code = text.charCodeAt(index);
		const glyph = Math.max(0, Math.min(94, code - 32));
		const cellX = (index % FRAME_COLUMNS) * CELL_WIDTH;
		const cellY = Math.floor(index / FRAME_COLUMNS) * CELL_HEIGHT;
		for (let row = 0; row < GLYPH_HEIGHT; row++) {
			const bits = FONT_ROWS[glyph * GLYPH_HEIGHT + row] ?? 0;
			for (let column = 0; column < GLYPH_WIDTH; column++) {
				if ((bits & (0x80 >> column)) === 0) continue;
				const x = cellX + column * 2;
				const y = cellY + row * 2;
				for (let dy = 0; dy < 2; dy++) {
					const at = (y + dy) * FRAME_WIDTH + x;
					pixels[at] = 0;
					pixels[at + 1] = 0;
				}
			}
		}
	}
	return pixels;
}

function encodePng(pixels: Buffer, width: number, height: number): Buffer {
	const scanlines = Buffer.alloc((width + 1) * height);
	for (let row = 0; row < height; row++) {
		pixels.copy(scanlines, row * (width + 1) + 1, row * width, (row + 1) * width);
	}
	const header = Buffer.alloc(13);
	header.writeUInt32BE(width, 0);
	header.writeUInt32BE(height, 4);
	header[8] = 8;
	header[9] = 0;
	return Buffer.concat([
		Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
		pngChunk("IHDR", header),
		pngChunk("IDAT", deflateSync(scanlines, { level: 9 })),
		pngChunk("IEND", Buffer.alloc(0)),
	]);
}

function pngChunk(type: string, data: Buffer): Buffer {
	const name = Buffer.from(type, "ascii");
	const length = Buffer.alloc(4);
	length.writeUInt32BE(data.length, 0);
	const checksum = Buffer.alloc(4);
	checksum.writeUInt32BE(crc32(Buffer.concat([name, data])), 0);
	return Buffer.concat([length, name, data, checksum]);
}

function crc32(data: Buffer): number {
	let crc = 0xffffffff;
	for (const byte of data) {
		crc ^= byte;
		for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
	}
	return (crc ^ 0xffffffff) >>> 0;
}
