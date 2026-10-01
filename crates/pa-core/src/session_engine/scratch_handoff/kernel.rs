use crate::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use crate::kernel::KernelShutdownOptions;
use crate::session_engine::tool_bridge::ToolDefinitionBridge;
use crate::tools::ipython::{create_ipython_tool_definition, IpythonToolOptions};
use pa_agent::agent::Agent;
use pa_agent::types::AgentTool;
use std::sync::Arc;

pub(super) const GUIDANCE: &str = "IPython is temporarily connected to a separate scratch-compaction kernel. The working Python kernel and its variables are retained for after compaction but are unavailable here. Only these calls are available: scratch_read(), scratch_write(text), and scratch_replace(old, new). They target only the handoff file named in this notice; do not pass a path. scratch_replace requires exactly one occurrence of old. Imports, variables, loops, shell commands, RLM, MCP, skills, and other tools are unavailable during closeout. Use the conversation evidence already available; record uncertainties instead of investigating. Save the checkpoint and finish.";

pub(super) struct ScratchKernel {
    provisioner: Arc<IpythonKernelProvisioner>,
    restore: Option<(Agent, Vec<Arc<dyn AgentTool>>)>,
}

impl ScratchKernel {
    pub(super) async fn install(
        agent: &Agent,
        cwd: &std::path::Path,
        path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let bootstrap = include_str!("kernel_bootstrap.py")
            .replace("__PATH__", &serde_json::to_string(&path.to_string_lossy())?);
        let provisioner = Arc::new(IpythonKernelProvisioner::new(
            cwd,
            IpythonKernelProvisionerOptions {
                bootstrap_code: Some(bootstrap),
                ..Default::default()
            },
        ));
        let mut tool = create_ipython_tool_definition(
            &cwd.to_string_lossy(),
            IpythonToolOptions {
                provisioner: provisioner.clone(),
                ui: None,
            },
        );
        tool.description = GUIDANCE.into();
        let prior = agent.state().await.tools;
        agent
            .set_tools(vec![Arc::new(ToolDefinitionBridge::new(tool))])
            .await;
        Ok(Self {
            provisioner,
            restore: Some((agent.clone(), prior)),
        })
    }

    pub(super) async fn finish(mut self) {
        if let Some((agent, tools)) = self.restore.take() {
            agent.set_tools(tools).await;
        }
        self.provisioner
            .dispose(Some(KernelShutdownOptions::default()))
            .await;
    }
}

impl Drop for ScratchKernel {
    fn drop(&mut self) {
        if let Some((agent, tools)) = self.restore.take() {
            let provisioner = self.provisioner.clone();
            tokio::spawn(async move {
                agent.set_tools(tools).await;
                provisioner
                    .dispose(Some(KernelShutdownOptions::default()))
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scratch_helpers_write_and_replace_only_the_bound_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.org");
        let bootstrap = include_str!("kernel_bootstrap.py").replace(
            "__PATH__",
            &serde_json::to_string(&path.to_string_lossy()).unwrap(),
        );
        let code = format!("{bootstrap}\nworking = 42\nscratch_write('* TODO active task')\nscratch_replace('TODO', 'DONE')\nassert working == 42\nassert scratch_read() == '* DONE active task'\ntry:\n    scratch_replace('missing', 'bad')\nexcept ValueError:\n    pass\nelse:\n    raise AssertionError('non-unique replacement accepted')\n");
        let output = std::process::Command::new("python3")
            .args(["-c", &code])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "* DONE active task");
    }
}
