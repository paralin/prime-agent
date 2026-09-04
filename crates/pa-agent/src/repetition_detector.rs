//! Bounded stream-side detection of repeated assistant text and thinking.

pub const DEFAULT_REPETITION_THRESHOLD: usize = 5;
const MAX_PERIOD: usize = 2048;
const WINDOW_UNITS: usize = 4096;
const BLOCK_BUFFER_UNITS: usize = 16_384;

pub struct RepetitionDetector {
    threshold: usize,
    runs: Vec<usize>,
    window: Vec<u16>,
    raw: Vec<u16>,
}

impl RepetitionDetector {
    #[must_use]
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(2),
            runs: vec![0; MAX_PERIOD + 1],
            window: Vec::new(),
            raw: Vec::new(),
        }
    }

    #[must_use]
    pub fn observe_text(&mut self, delta: &str) -> bool {
        if self.observe_periodic(delta) {
            return true;
        }
        self.raw.extend(delta.encode_utf16());
        if self.raw.len() > 2 * BLOCK_BUFFER_UNITS {
            self.raw.drain(..self.raw.len() - BLOCK_BUFFER_UNITS);
        }
        let raw = String::from_utf16_lossy(&self.raw);
        let lines: Vec<_> = raw
            .split('\n')
            .map(normalize_line)
            .filter(|line| line.encode_utf16().count() >= 40)
            .collect();
        if repeated_tail(&lines, 6) {
            return true;
        }
        let mut paragraphs = Vec::new();
        let mut paragraph = String::new();
        for line in raw.split('\n') {
            if line.trim_matches([' ', '\t']).is_empty() {
                let value = normalize_line(&paragraph);
                if value.encode_utf16().count() >= 60 {
                    paragraphs.push(value);
                }
                paragraph.clear();
            } else {
                if !paragraph.is_empty() {
                    paragraph.push('\n');
                }
                paragraph.push_str(line);
            }
        }
        let value = normalize_line(&paragraph);
        if value.encode_utf16().count() >= 60 {
            paragraphs.push(value);
        }
        repeated_tail(&paragraphs, 3)
    }

    fn observe_periodic(&mut self, delta: &str) -> bool {
        for character in delta.chars() {
            let normalized = if character.is_whitespace() {
                if self.window.last() == Some(&u16::from(b' ')) {
                    continue;
                }
                " ".to_string()
            } else {
                character.to_lowercase().collect()
            };
            self.window.extend(normalized.encode_utf16());
            if self.window.len() >= 2 * WINDOW_UNITS {
                self.window.drain(..self.window.len() - WINDOW_UNITS);
            }
            let index = self.window.len() - 1;
            for period in 4..=MAX_PERIOD.min(index) {
                if self.window[index] == self.window[index - period] {
                    self.runs[period] += 1;
                    let needed = self
                        .threshold
                        .saturating_sub(2)
                        .saturating_mul(period)
                        .saturating_add(1);
                    if self.runs[period] >= needed
                        && String::from_utf16_lossy(&self.window[index + 1 - period..])
                            .chars()
                            .any(char::is_alphanumeric)
                    {
                        return true;
                    }
                } else {
                    self.runs[period] = 0;
                }
            }
        }
        false
    }
}

fn normalize_line(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn repeated_tail(values: &[String], copies: usize) -> bool {
    values.len() >= copies
        && values[values.len() - copies..]
            .iter()
            .all(|value| Some(value) == values.last())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeds(text: &str, threshold: usize) -> bool {
        let mut detector = RepetitionDetector::new(threshold);
        let chars: Vec<_> = text.chars().collect();
        chars
            .chunks(7)
            .any(|chunk| detector.observe_text(&chunk.iter().collect::<String>()))
    }

    #[test]
    fn detects_three_normalized_thought_paragraphs() {
        let block = " But wait — actually could a legit assistant output 6 identical lines of code? Rarely. But is it a false positive risk?\n Actually identical consecutive code lines doing the same thing is weird.";
        assert!(feeds(&[block, block, block].join("\n\n"), 5));
    }
    #[test]
    fn periodic_blocks_require_the_configured_copy_count() {
        let block = "The parser should stop when the same unique paragraph comes back again without new evidence. ";
        assert!(!feeds(&block.repeat(3), 5));
        assert!(feeds(&block.repeat(6), 5));
        assert!(!feeds(&"repetitive block ".repeat(4), 5));
        assert!(feeds(&"repetitive block ".repeat(2), 2));
    }
    #[test]
    fn short_sentences_and_long_lines_detect_only_degenerate_repetition() {
        assert!(!feeds(
            &"The file roadmap.org is at the repo root. ".repeat(3),
            5
        ));
        assert!(feeds(
            &"The roadmap file lives at the repository root. ".repeat(6),
            5
        ));
        let line = "    const repeatedBinding = waitForIdle(controller, timeoutMs);";
        assert!(feeds(&[line; 6].join("\n"), 5));
    }
    #[test]
    fn ordinary_output_and_punctuation_rules_do_not_trigger() {
        assert!(!feeds("The parser reads the header first. Then it validates the checksum. Finally it returns the payload. ", 5));
        assert!(!feeds(&"-".repeat(40), 5));
        assert!(!feeds(
            "    const x = 1;\n    const y = 2;\n    const z = 3;\n",
            5
        ));
        let section = "Executor re-verification: before each action, cheap re-checks:\n- delete: verify merged; skip with a failure message if it fails.\n- prune: verify still junk-only by checking status.\n- commit: verify branch not protected.\n- merge: verify main checkout clean and on default.\n\nWorktree removal: git worktree remove from the common directory. If dirty, it fails.\nThe planner never emits a delete action for dirty worktrees.\n\nOrdering: commit, push, merge, review, delete, prune.\n";
        assert!(!feeds(&format!("{section}\n\n{section}\n"), 5));
    }
    #[test]
    fn normalization_and_window_trimming_preserve_stream_detection() {
        let mut detector = RepetitionDetector::new(5);
        for i in 0..3000 {
            assert!(!detector.observe_text(&format!("unique {i:x} payload {} ", i * 31)));
        }
        assert!(detector.observe_text(&"Mixed CASE   repetition block ".repeat(6)));
        assert!(detector.window.len() <= 2 * WINDOW_UNITS);
        assert!(detector.raw.len() <= 2 * BLOCK_BUFFER_UNITS);
    }
}
