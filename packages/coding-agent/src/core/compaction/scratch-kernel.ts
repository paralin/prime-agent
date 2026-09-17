import { Type } from "typebox";

import { createIpythonTool, IpythonKernelProvisioner } from "../tools/ipython.js";
import { resolveToCwd } from "../tools/path-utils.js";

export const SCRATCH_KERNEL_GUIDANCE =
	"IPython is temporarily connected to a separate scratch-compaction kernel. The working Python kernel and its variables are retained for after compaction but are unavailable here. Only these calls are available: scratch_read(), scratch_write(text), and scratch_replace(old, new). They target only the handoff file named in this notice; do not pass a path. scratch_replace requires exactly one occurrence of old. Imports, variables, loops, shell commands, RLM, MCP, skills, and other tools are unavailable during closeout. Use the conversation evidence already available; record uncertainties instead of investigating. Save the checkpoint and finish.";

/** ScratchKernel owns a lazy, non-persistent kernel for one closeout episode. */
export class ScratchKernel {
	private readonly provisioner: IpythonKernelProvisioner;
	readonly tool: ReturnType<typeof createIpythonTool>;

	constructor(cwd: string, displayPath: string) {
		this.provisioner = new IpythonKernelProvisioner(cwd, {
			bootstrapCode: buildScratchBootstrap(resolveToCwd(displayPath, cwd)),
		});
		const python = createIpythonTool(cwd, { provisioner: this.provisioner });
		this.tool = {
			...python,
			description: SCRATCH_KERNEL_GUIDANCE,
			parameters: Type.Object({
				code: Type.String({
					description: "One or more scratch_read(), scratch_write(text), or scratch_replace(old, new) calls.",
				}),
			}),
			execute: (id, params, signal, onUpdate) => python.execute(id, { code: params.code }, signal, onUpdate),
		};
	}

	/** dispose stops only the closeout kernel; the working kernel is never touched. */
	dispose(): Promise<void> {
		return this.provisioner.dispose({ snapshot: false });
	}
}

/** buildScratchBootstrap defines the checkpoint helpers; ordinary Python evaluates the closeout code. */
function buildScratchBootstrap(absolutePath: string): string {
	return `
import os as _scratch_os
from pathlib import Path as _ScratchPath
from tempfile import NamedTemporaryFile as _ScratchTemporaryFile

class _ScratchEditor:
    def __init__(self, path: str):
        self._path = _ScratchPath(path)

    def read(self) -> str:
        try:
            return self._path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return ""

    def write(self, text: str) -> str:
        if not text.strip():
            raise ValueError("The handoff checkpoint must not be empty")
        self._path.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with _ScratchTemporaryFile(mode="w", encoding="utf-8", dir=self._path.parent, prefix=".scratch-", delete=False) as output:
                temporary = _ScratchPath(output.name)
                output.write(text)
            _scratch_os.replace(temporary, self._path)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        return "Saved handoff checkpoint"

    def replace(self, old: str, new: str) -> str:
        text = self.read()
        if not old or text.count(old) != 1:
            raise ValueError("scratch_replace requires exactly one occurrence of old; use scratch_read() first")
        return self.write(text.replace(old, new, 1))

_scratch_editor = _ScratchEditor(${JSON.stringify(absolutePath)})
scratch_read = _scratch_editor.read
scratch_write = _scratch_editor.write
scratch_replace = _scratch_editor.replace
`.trim();
}
