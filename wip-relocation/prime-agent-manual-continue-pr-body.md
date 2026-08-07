A lone `.` submission now resumes the most recent intent through a hidden host-authored continuation. The session persists the hidden custom message and presents the continuation directive to the model without adding a visible user turn.

Normalize the input at the AgentSession boundary so interactive, daemon, and SDK prompts agree. Keep image-bearing dot prompts unchanged, omit continuation submissions from editor history, and document the interactive behavior.
