use std::sync::Arc;
use pa_agent::agent::Agent;
use pa_agent::types::AgentTool;
use crate::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use crate::kernel::KernelShutdownOptions;
use crate::tools::ipython::{create_ipython_tool_definition, IpythonToolOptions};
use crate::session_engine::tool_bridge::ToolDefinitionBridge;

pub(super) const GUIDANCE: &str = "IPython is temporarily connected to a separate scratch-compaction kernel. The working Python kernel and its variables are retained for after compaction but are unavailable here. Only these calls are allowed: scratch_read(), scratch_write(text), and scratch_replace(old, new). They target only the handoff file named in this notice; do not pass a path. Use literal strings (triple-quoted strings are supported), with one or more calls per cell. scratch_replace requires exactly one occurrence of old. Imports, variables, loops, shell commands, RLM, MCP, skills, and other tools are unavailable during closeout. Use the conversation evidence already available; record uncertainties instead of investigating. Save the checkpoint and finish.";

pub(super) struct ScratchKernel {
    provisioner: Arc<IpythonKernelProvisioner>,
    restore: Option<(Agent, Vec<Arc<dyn AgentTool>>)>,
}

impl ScratchKernel {
    pub(super) async fn install(agent: &Agent, cwd: &std::path::Path, path: &std::path::Path) -> anyhow::Result<Self> {
        let bootstrap = include_str!("kernel_bootstrap.py").replace("__PATH__", &serde_json::to_string(&path.to_string_lossy())?);
        let provisioner = Arc::new(IpythonKernelProvisioner::new(cwd, IpythonKernelProvisionerOptions {
            bootstrap_code: Some(bootstrap), ..Default::default()
        }));
        let mut tool = create_ipython_tool_definition(&cwd.to_string_lossy(), IpythonToolOptions { provisioner: provisioner.clone(), ui: None });
        tool.description = GUIDANCE.into();
        let execute = tool.execute.clone();
        tool.execute = Arc::new(move |id, params, signal, update| {
            let code = params.get("code").and_then(serde_json::Value::as_str).unwrap_or_default();
            execute(id, serde_json::json!({"code":format!("_scratch_execute({})", serde_json::to_string(code).expect("string serialization"))}), signal, update)
        });
        let prior = agent.state().await.tools;
        agent.set_tools(vec![Arc::new(ToolDefinitionBridge::new(tool))]).await;
        Ok(Self { provisioner, restore: Some((agent.clone(), prior)) })
    }

    pub(super) async fn finish(mut self) {
        if let Some((agent, tools)) = self.restore.take() { agent.set_tools(tools).await; }
        self.provisioner.dispose(Some(KernelShutdownOptions::default())).await;
    }
}

impl Drop for ScratchKernel {
    fn drop(&mut self) {
        if let Some((agent, tools)) = self.restore.take() {
            let provisioner = self.provisioner.clone();
            tokio::spawn(async move {
                agent.set_tools(tools).await;
                provisioner.dispose(Some(KernelShutdownOptions::default())).await;
            });
        }
    }
}
