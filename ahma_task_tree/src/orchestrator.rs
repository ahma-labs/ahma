use anyhow::{Context, Result, anyhow};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use ahma_llm_monitor::LlmClient;
use ahma_mcp::Adapter;
use ahma_mcp::egress::{EgressAllowlist, EgressProxy, EgressProxyConfig};
use ahma_mcp::sandbox::Sandbox;

use crate::config::TaskTreeConfig;
use crate::parser::parse_steps;
use crate::prompt::{build_planning_prompt, build_summarisation_prompt};
use crate::tree::{NodeId, NodeResult, TaskTree, TaskType};

pub struct TaskTreeOrchestrator {
    config: TaskTreeConfig,
    adapter: Arc<Adapter>,
    llm: Arc<LlmClient>,
}

impl TaskTreeOrchestrator {
    pub fn new(config: TaskTreeConfig, adapter: Arc<Adapter>) -> Result<Self> {
        let resolved = config.llm_provider.resolve()?;
        let llm = Arc::new(LlmClient::new(
            resolved.base_url,
            resolved.model,
            resolved.api_key,
        ));
        Ok(Self {
            config,
            adapter,
            llm,
        })
    }

    /// Run the root task tree execution for a given goal.
    pub async fn execute(&self, goal: &str) -> Result<NodeResult> {
        info!("TaskTreeOrchestrator starting execution for goal: {}", goal);
        let mut tree = TaskTree::new();

        let root_scopes = self
            .adapter
            .sandbox()
            .scopes()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let root_id = tree.add_node(
            None,
            goal.to_string(),
            TaskType::Planning,
            Some(root_scopes),
            None,
            None,
        );

        let result = self.execute_node(&mut tree, root_id).await?;
        if result.success {
            info!("TaskTreeOrchestrator completed successfully");
        } else {
            warn!("TaskTreeOrchestrator execution failed");
        }
        Ok(result)
    }

    /// Recursively execute a node in the tree.
    pub fn execute_node<'a>(
        &'a self,
        tree: &'a mut TaskTree,
        node_id: NodeId,
    ) -> futures::future::BoxFuture<'a, Result<NodeResult>> {
        use futures::future::FutureExt;
        async move {
            let node = tree
                .get_node(node_id)
                .ok_or_else(|| anyhow!("Node not found"))?
                .clone();

            {
                let n = tree.get_node_mut(node_id).unwrap();
                if let Err(e) = n.start() {
                    warn!(node = node_id.0, %e, "Unexpected node state transition");
                }
            }

            let result = match &node.node_type {
                TaskType::Planning => {
                    self.execute_planning_node(tree, node_id, node.clone())
                        .await
                }
                TaskType::ShellCommand { command } => {
                    self.execute_shell_command_node(tree, node_id, command)
                        .await
                }
                TaskType::LlmCall { instructions } => {
                    self.execute_llm_call_node(tree, node_id, instructions)
                        .await
                }
            }?;

            {
                let n = tree.get_node_mut(node_id).unwrap();
                let outcome = if result.success {
                    n.complete(result.clone())
                } else {
                    n.fail(result.clone())
                };
                if let Err(e) = outcome {
                    warn!(node = node_id.0, %e, "Unexpected node state transition");
                }
            }

            Ok(result)
        }
        .boxed()
    }

    async fn plan_and_create_children(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        node: &crate::tree::TaskNode,
        max_subtasks: usize,
        goal: &str,
    ) -> Result<Vec<NodeId>> {
        let branch_context = self.build_branch_context(tree, node_id);
        let prompt =
            build_planning_prompt(goal, &node.task_description, &branch_context, max_subtasks);

        let system_msg = json!({
            "role": "system",
            "content": "You are a precise task orchestrator. You decompose goals into subtasks and output strictly valid JSON according to the schema provided."
        });
        let user_msg = json!({
            "role": "user",
            "content": prompt
        });
        let messages = vec![system_msg, user_msg];

        let timeout = Duration::from_secs(self.config.llm_timeout_seconds.unwrap_or(30));
        let completion_res =
            tokio::time::timeout(timeout, self.llm.chat_completion_with_tools(messages, &[])).await;

        let completion = match completion_res {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(anyhow!("LLM call failed: {}", e)),
            Err(_) => return Err(anyhow!("LLM call timed out")),
        };

        let parsed_steps = parse_steps(&completion.content)
            .context("Failed to parse LLM planning response steps")?;

        info!(
            "Parsed {} steps for planning node {}",
            parsed_steps.len(),
            node_id.0
        );

        let mut child_ids = Vec::new();
        let parent_scopes = self.get_effective_parent_scopes(tree, node_id);
        let parent_tools = self.get_effective_parent_allowed_tools(tree, node_id);
        let parent_domains = self.get_effective_parent_allowed_domains(tree, node_id);

        for step in parsed_steps {
            let child_id = self.create_child_node(
                tree,
                node_id,
                &step,
                &parent_scopes,
                &parent_tools,
                &parent_domains,
                false,
            )?;
            child_ids.push(child_id);
        }

        Ok(child_ids)
    }

    /// Try to execute a single child node with retries. Returns the task summary on success,
    /// or `None` if all attempts failed.
    async fn execute_child_with_retries(
        &self,
        tree: &mut TaskTree,
        child_id: NodeId,
        max_retries: usize,
    ) -> Option<String> {
        for retry in 0..=max_retries {
            if retry > 0 {
                info!(
                    "Retrying child node {} (attempt {}/{})",
                    child_id.0, retry, max_retries
                );
                if let Some(child_node) = tree.get_node_mut(child_id)
                    && let Err(e) = child_node.reset_for_retry(retry)
                {
                    warn!(node = child_id.0, %e, "Unexpected node state transition");
                }
            }

            match self.execute_node(tree, child_id).await {
                Ok(child_res) if child_res.success => {
                    let desc = tree
                        .get_node(child_id)
                        .map(|n| n.task_description.as_str())
                        .unwrap_or("");
                    return Some(format!("Task '{}': {}", desc, child_res.summary));
                }
                Ok(child_res) => {
                    warn!("Child node {} failed: {}", child_id.0, child_res.summary);
                }
                Err(e) => {
                    warn!("Error executing child node {}: {}", child_id.0, e);
                }
            }
        }
        None
    }

    async fn execute_child_queue(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        node: &crate::tree::TaskNode,
        child_ids: Vec<NodeId>,
        max_subtasks: usize,
        goal: &str,
    ) -> Result<(bool, Vec<String>)> {
        let parent_scopes = self.get_effective_parent_scopes(tree, node_id);
        let parent_tools = self.get_effective_parent_allowed_tools(tree, node_id);
        let parent_domains = self.get_effective_parent_allowed_domains(tree, node_id);

        let mut child_summaries = Vec::new();
        let max_retries = self.config.max_retries.unwrap_or(2);
        let mut queue = std::collections::VecDeque::from(child_ids);

        while let Some(child_id) = queue.pop_front() {
            if let Some(summary) = self
                .execute_child_with_retries(tree, child_id, max_retries)
                .await
            {
                child_summaries.push(summary);
                continue;
            }

            // All retries exhausted — attempt recovery/backtracking.
            let recovered = self
                .handle_recovery_backtracking(
                    tree,
                    node_id,
                    node,
                    child_id,
                    &mut queue,
                    max_subtasks,
                    goal,
                    &parent_scopes,
                    &parent_tools,
                    &parent_domains,
                )
                .await?;

            if !recovered {
                return Ok((false, child_summaries));
            }
        }

        Ok((true, child_summaries))
    }

    async fn execute_planning_node(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        node: crate::tree::TaskNode,
    ) -> Result<NodeResult> {
        let max_subtasks = self.config.max_depth.unwrap_or(4);
        let goal = tree
            .root_id
            .and_then(|id| tree.get_node(id))
            .map(|n| n.task_description.clone())
            .unwrap_or_else(|| node.task_description.clone());

        info!(
            "Decomposing planning node {} (description: {})",
            node_id.0, node.task_description
        );

        let child_ids = self
            .plan_and_create_children(tree, node_id, &node, max_subtasks, &goal)
            .await?;

        let (overall_success, child_summaries) = self
            .execute_child_queue(tree, node_id, &node, child_ids, max_subtasks, &goal)
            .await?;

        let summary = if overall_success {
            let joined_summaries = child_summaries.join("\n");
            if joined_summaries.len() > self.config.summarisation_threshold.unwrap_or(500) {
                self.summarize_text(&joined_summaries, "").await?
            } else {
                joined_summaries
            }
        } else {
            "One or more subtasks failed.".to_string()
        };

        Ok(NodeResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            summary,
            success: overall_success,
        })
    }

    fn security_violation_msg(
        context: &str,
        kind: &str,
        value: &str,
        permitted: &[String],
    ) -> String {
        format!(
            "Security violation{}: Child task specifies allowed {} {:?} which is not permitted by parent allowed {}s {:?}",
            context, kind, value, kind, permitted
        )
    }

    fn validate_child_tools(
        child_tools: &[String],
        parent_tools: &Option<Vec<String>>,
        is_replan: bool,
    ) -> Result<()> {
        let Some(p_tools) = parent_tools else {
            return Ok(());
        };
        let context = if is_replan { " during re-plan" } else { "" };
        for tool in child_tools {
            if !p_tools.iter().any(|pt| tool == pt || tool.starts_with(pt)) {
                return Err(anyhow!(Self::security_violation_msg(
                    context, "tool", tool, p_tools
                )));
            }
        }
        Ok(())
    }

    fn validate_child_domains(
        child_domains: &[String],
        parent_domains: &Option<Vec<String>>,
        is_replan: bool,
    ) -> Result<()> {
        let Some(p_domains) = parent_domains else {
            return Ok(());
        };
        let context = if is_replan { " during re-plan" } else { "" };
        for domain in child_domains {
            if !p_domains.contains(domain) {
                return Err(anyhow!(Self::security_violation_msg(
                    context, "domain", domain, p_domains
                )));
            }
        }
        Ok(())
    }

    fn map_step_task_type(
        step_type: &str,
        command: Option<String>,
        instructions: Option<String>,
        is_replan: bool,
    ) -> Result<TaskType> {
        match step_type {
            "shell_command" => Ok(TaskType::ShellCommand {
                command: command.unwrap_or_default(),
            }),
            "llm_call" => Ok(TaskType::LlmCall {
                instructions: instructions.unwrap_or_default(),
            }),
            "planning" => Ok(TaskType::Planning),
            other => {
                let msg = if is_replan {
                    format!("Unsupported task type returned by LLM recovery: {}", other)
                } else {
                    format!("Unsupported task type returned by LLM: {}", other)
                };
                Err(anyhow!(msg))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_child_node(
        &self,
        tree: &mut TaskTree,
        parent_id: NodeId,
        step: &crate::parser::ParsedStep,
        parent_scopes: &[PathBuf],
        parent_tools: &Option<Vec<String>>,
        parent_domains: &Option<Vec<String>>,
        is_replan: bool,
    ) -> Result<NodeId> {
        let child_scopes = if let Some(ref step_scopes) = step.sandbox_scopes {
            let resolved = self.validate_child_scopes(step_scopes, parent_scopes)?;
            let resolved_strs = resolved
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            Some(resolved_strs)
        } else {
            let parent_strs = parent_scopes
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            Some(parent_strs)
        };

        let child_tools = if let Some(step_tools) = &step.allowed_tools {
            Self::validate_child_tools(step_tools, parent_tools, is_replan)?;
            Some(step_tools.clone())
        } else {
            parent_tools.clone()
        };

        let child_domains = if let Some(step_domains) = &step.allowed_domains {
            Self::validate_child_domains(step_domains, parent_domains, is_replan)?;
            Some(step_domains.clone())
        } else {
            parent_domains.clone()
        };

        let task_type = Self::map_step_task_type(
            step.r#type.as_str(),
            step.command.clone(),
            step.instructions.clone(),
            is_replan,
        )?;

        let child_id = tree.add_node(
            Some(parent_id),
            step.task.clone(),
            task_type,
            child_scopes,
            child_tools,
            child_domains,
        );
        Ok(child_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_recovery_backtracking(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        node: &crate::tree::TaskNode,
        child_id: NodeId,
        queue: &mut std::collections::VecDeque<NodeId>,
        max_subtasks: usize,
        goal: &str,
        parent_scopes: &[PathBuf],
        parent_tools: &Option<Vec<String>>,
        parent_domains: &Option<Vec<String>>,
    ) -> Result<bool> {
        info!(
            "Child node {} failed after maximum retries. Initiating recovery/backtracking...",
            child_id.0
        );

        // Collect descriptions of remaining unexecuted steps
        let mut remaining_descs = Vec::new();
        for &rem_id in queue.iter() {
            if let Some(n) = tree.get_node(rem_id) {
                remaining_descs.push(n.task_description.clone());
            }
        }

        // Format the failed node outcome and extract description
        let (failed_node_desc, outcome_str) = {
            let failed_node = tree.get_node(child_id).unwrap();
            let outcome = failed_node
                .result
                .as_ref()
                .map(|r| {
                    format!(
                        "Exit code: {:?}\nSummary: {}\nStdout: {}\nStderr: {}",
                        r.exit_code, r.summary, r.stdout, r.stderr
                    )
                })
                .unwrap_or_else(|| "No outcome recorded".to_string());
            (failed_node.task_description.clone(), outcome)
        };

        // Mark parent planning node state as Backtracking
        if let Some(n) = tree.get_node_mut(node_id)
            && let Err(e) = n.begin_backtracking()
        {
            warn!(node = node_id.0, %e, "Unexpected node state transition");
        }

        let branch_context = self.build_branch_context(tree, node_id);
        let recovery_prompt = crate::prompt::build_recovery_prompt(
            goal,
            &node.task_description,
            &branch_context,
            &failed_node_desc,
            &outcome_str,
            &remaining_descs,
            max_subtasks,
        );

        let system_msg = json!({
            "role": "system",
            "content": "You are a precise task orchestrator. You handle subtask failures and decide how to recover."
        });
        let user_msg = json!({
            "role": "user",
            "content": recovery_prompt
        });

        let recovery_timeout = Duration::from_secs(self.config.llm_timeout_seconds.unwrap_or(30));
        let recovery_completion_res = tokio::time::timeout(
            recovery_timeout,
            self.llm
                .chat_completion_with_tools(vec![system_msg, user_msg], &[]),
        )
        .await;

        let recovery_completion = match recovery_completion_res {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(anyhow!("LLM call failed during recovery: {}", e)),
            Err(_) => return Err(anyhow!("LLM call timed out during recovery")),
        };

        let decision = crate::parser::parse_recovery_decision(&recovery_completion.content)
            .context("Failed to parse LLM recovery decision")?;

        info!(
            "Recovery decision for node {}: action={}",
            node_id.0, decision.action
        );

        if decision.action != "re_plan" {
            return Ok(false);
        }
        let Some(new_steps) = decision.steps else {
            return Ok(false);
        };
        info!(
            "Re-planning node {} with {} new steps",
            node_id.0,
            new_steps.len()
        );
        // Clear unexecuted remaining steps from queue
        queue.clear();

        let mut new_child_ids = Vec::new();
        for step in new_steps {
            let child_id = self.create_child_node(
                tree,
                node_id,
                &step,
                parent_scopes,
                parent_tools,
                parent_domains,
                true,
            )?;
            new_child_ids.push(child_id);
        }

        queue.extend(new_child_ids);

        // Resume parent node: Backtracking -> Running
        if let Some(n) = tree.get_node_mut(node_id)
            && let Err(e) = n.start()
        {
            warn!(node = node_id.0, %e, "Unexpected node state transition");
        }

        Ok(true)
    }

    /// Enforce that `command` matches one of the parent's allowed tool prefixes.
    /// A `None` policy means no restriction.
    fn enforce_command_allowed(command: &str, parent_tools: &Option<Vec<String>>) -> Result<()> {
        let Some(allowed_tools) = parent_tools else {
            return Ok(());
        };
        let trimmed_cmd = command.trim();
        let is_allowed = allowed_tools.iter().any(|prefix| {
            trimmed_cmd == prefix || trimmed_cmd.starts_with(&format!("{} ", prefix))
        });
        if !is_allowed {
            return Err(anyhow!(
                "Security policy block: Command {:?} rejected. It does not match allowed tools prefixes {:?}",
                command,
                allowed_tools
            ));
        }
        Ok(())
    }

    async fn execute_shell_command_node(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        command: &str,
    ) -> Result<NodeResult> {
        let parent_tools = self.get_effective_parent_allowed_tools(tree, node_id);
        Self::enforce_command_allowed(command, &parent_tools)?;

        let resolved_scopes = self.get_effective_parent_scopes(tree, node_id);
        let working_dir = resolved_scopes.first().cloned().unwrap_or_else(|| {
            self.adapter
                .sandbox()
                .scopes()
                .first()
                .cloned()
                .unwrap_or_else(|| std::env::current_dir().unwrap())
        });

        info!(
            "Running shell command node {} under sandbox scopes {:?}: {:?}",
            node_id.0, resolved_scopes, command
        );

        let parent_domains = self.get_effective_parent_allowed_domains(tree, node_id);
        let proxy = if let Some(ref domains) = parent_domains {
            info!("Starting egress proxy restricted to domains: {:?}", domains);
            let allowlist = EgressAllowlist::from_str(&domains.join("\n"));
            // block_private defaults to true: an allowlisted domain that resolves
            // to a private/loopback address is refused at connect time (SSRF).
            let proxy_config = EgressProxyConfig {
                allowlist,
                ..Default::default()
            };
            let p = EgressProxy::start(proxy_config).await?;
            Some(p)
        } else {
            None
        };

        let temp_file_mgr = ahma_mcp::adapter::TempFileManager::new();
        let (program, args_vec) = ahma_mcp::adapter::prepare_command_and_args(
            command,
            None,
            None,
            &working_dir,
            &temp_file_mgr,
        )
        .await?;

        let current_sandbox = Sandbox::new(
            resolved_scopes,
            self.adapter.sandbox().mode(),
            self.adapter.sandbox().is_no_temp_files(),
            false,
            self.adapter.sandbox().is_tmp_access(),
        )?;

        let mut cmd = current_sandbox.create_command(&program, &args_vec, &working_dir)?;

        if let Some(ref p) = proxy {
            cmd.envs(p.env_vars());
        }

        let timeout_duration = Duration::from_secs(self.config.llm_timeout_seconds.unwrap_or(30));
        let output_res = tokio::time::timeout(timeout_duration, cmd.output()).await;

        let output = match output_res {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(anyhow!("Command execution failed: {}", e)),
            Err(_) => return Err(anyhow!("Command execution timed out")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let success = output.status.success();
        let exit_code = output.status.code();

        let total_len = stdout.len() + stderr.len();
        let summary = if total_len > self.config.summarisation_threshold.unwrap_or(500) {
            self.summarize_text(&stdout, &stderr).await?
        } else {
            format!("{}{}", stdout, stderr)
        };

        Ok(NodeResult {
            exit_code,
            stdout,
            stderr,
            summary,
            success,
        })
    }

    async fn execute_llm_call_node(
        &self,
        tree: &mut TaskTree,
        node_id: NodeId,
        instructions: &str,
    ) -> Result<NodeResult> {
        info!("Executing LLM call node {}: {}", node_id.0, instructions);
        let branch_context = self.build_branch_context(tree, node_id);
        let system_msg = json!({
            "role": "system",
            "content": "You are a reasoning agent performing a subtask. Read the context and instructions carefully, then execute the step and summarize the result."
        });
        let user_msg = json!({
            "role": "user",
            "content": format!(
                "Instructions:\n{}\n\nContext (Previous outcomes):\n{}",
                instructions, branch_context
            )
        });

        let timeout = Duration::from_secs(self.config.llm_timeout_seconds.unwrap_or(30));
        let completion_res = tokio::time::timeout(
            timeout,
            self.llm
                .chat_completion_with_tools(vec![system_msg, user_msg], &[]),
        )
        .await;

        let completion = match completion_res {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(anyhow!("LLM call failed: {}", e)),
            Err(_) => return Err(anyhow!("LLM call timed out")),
        };

        Ok(NodeResult {
            exit_code: None,
            stdout: completion.content.clone(),
            stderr: String::new(),
            summary: completion.content,
            success: true,
        })
    }

    /// Collect ancestor nodes from root down to the immediate parent of `node_id`.
    fn collect_ancestors(tree: &TaskTree, node_id: NodeId) -> Vec<&crate::tree::TaskNode> {
        let mut ancestors = Vec::new();
        let mut curr = tree.get_node(node_id);
        while let Some(node) = curr {
            let Some(parent_id) = node.parent_id else {
                break;
            };
            let Some(parent) = tree.get_node(parent_id) else {
                break;
            };
            ancestors.push(parent);
            curr = Some(parent);
        }
        ancestors.reverse();
        ancestors
    }

    /// Collect completed sibling nodes that appear before `node_id` in the parent's child list.
    fn collect_prior_siblings(tree: &TaskTree, node_id: NodeId) -> Vec<&crate::tree::TaskNode> {
        let parent = tree
            .get_node(node_id)
            .and_then(|n| n.parent_id)
            .and_then(|pid| tree.get_node(pid));
        let Some(parent) = parent else {
            return Vec::new();
        };
        parent
            .children
            .iter()
            .take_while(|&&id| id != node_id)
            .filter_map(|&id| tree.get_node(id))
            .collect()
    }

    fn build_branch_context(&self, tree: &TaskTree, node_id: NodeId) -> String {
        let mut context = Vec::new();

        for ancestor in Self::collect_ancestors(tree, node_id) {
            let summary = ancestor
                .result
                .as_ref()
                .map(|r| r.summary.as_str())
                .unwrap_or("In progress");
            context.push(format!(
                "Ancestor Task [{}]: {}\nSummary: {}",
                ancestor.id.0, ancestor.task_description, summary
            ));
        }

        for sib in Self::collect_prior_siblings(tree, node_id) {
            let outcome = sib
                .result
                .as_ref()
                .map(|r| r.summary.as_str())
                .unwrap_or("No summary available");
            context.push(format!(
                "Completed Sibling Task [{}]: {}\nOutcome: {}",
                sib.id.0, sib.task_description, outcome
            ));
        }

        if context.is_empty() {
            "No prior task context available.".to_string()
        } else {
            context.join("\n\n")
        }
    }

    /// Resolve a list of scope strings against a base path, canonicalising each entry.
    fn resolve_scopes(&self, scopes: &[String]) -> Vec<PathBuf> {
        let base_scope = self
            .adapter
            .sandbox()
            .scopes()
            .first()
            .cloned()
            .unwrap_or_default();
        scopes
            .iter()
            .map(|s| {
                let p = PathBuf::from(s);
                let full = if p.is_absolute() {
                    p
                } else {
                    base_scope.join(p)
                };
                dunce::canonicalize(&full).unwrap_or(full)
            })
            .collect()
    }

    fn get_effective_parent_scopes(&self, tree: &TaskTree, node_id: NodeId) -> Vec<PathBuf> {
        let mut curr_node_id = Some(node_id);
        while let Some(curr_id) = curr_node_id {
            let Some(node) = tree.get_node(curr_id) else {
                break;
            };
            if let Some(ref scopes) = node.sandbox_scopes {
                return self.resolve_scopes(scopes);
            }
            curr_node_id = node.parent_id;
        }
        self.adapter.sandbox().scopes().to_vec()
    }

    fn validate_child_scopes(
        &self,
        child_scopes: &[String],
        parent_scopes: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        let mut resolved_child_scopes = Vec::new();
        for scope_str in child_scopes {
            let child_path = PathBuf::from(scope_str);
            let base_path = parent_scopes
                .first()
                .ok_or_else(|| anyhow!("No parent sandbox scopes configured"))?;
            let full_child_path = if child_path.is_absolute() {
                child_path
            } else {
                base_path.join(child_path)
            };

            let canonical_child = dunce::canonicalize(&full_child_path)
                .unwrap_or_else(|_| ahma_mcp::sandbox::normalize_path_lexically(&full_child_path));

            let is_allowed = parent_scopes
                .iter()
                .any(|parent| canonical_child.starts_with(parent));

            if !is_allowed {
                return Err(anyhow!(
                    "Security violation: Sandbox scope escape attempt! Child scope {:?} is not within parent scopes {:?}",
                    canonical_child,
                    parent_scopes
                ));
            }
            resolved_child_scopes.push(canonical_child);
        }
        Ok(resolved_child_scopes)
    }

    fn get_effective_parent_allowed_tools(
        &self,
        tree: &TaskTree,
        node_id: NodeId,
    ) -> Option<Vec<String>> {
        let mut curr_node_id = Some(node_id);
        while let Some(curr_id) = curr_node_id {
            if let Some(node) = tree.get_node(curr_id) {
                if let Some(ref tools) = node.allowed_tools {
                    return Some(tools.clone());
                }
                curr_node_id = node.parent_id;
            } else {
                break;
            }
        }
        None
    }

    fn get_effective_parent_allowed_domains(
        &self,
        tree: &TaskTree,
        node_id: NodeId,
    ) -> Option<Vec<String>> {
        let mut curr_node_id = Some(node_id);
        while let Some(curr_id) = curr_node_id {
            if let Some(node) = tree.get_node(curr_id) {
                if let Some(ref domains) = node.allowed_domains {
                    return Some(domains.clone());
                }
                curr_node_id = node.parent_id;
            } else {
                break;
            }
        }
        None
    }

    async fn summarize_text(&self, stdout: &str, stderr: &str) -> Result<String> {
        let prompt = build_summarisation_prompt(stdout, stderr);
        let system_msg = json!({
            "role": "system",
            "content": "You are a concise summarizer. Read the stdout and stderr, then reply with a summary in 3 sentences or less."
        });
        let user_msg = json!({
            "role": "user",
            "content": prompt
        });
        let timeout = Duration::from_secs(self.config.llm_timeout_seconds.unwrap_or(30));
        let completion_res = tokio::time::timeout(
            timeout,
            self.llm
                .chat_completion_with_tools(vec![system_msg, user_msg], &[]),
        )
        .await;

        let completion = match completion_res {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(anyhow!("LLM summarization failed: {}", e)),
            Err(_) => return Err(anyhow!("LLM summarization timed out")),
        };
        Ok(completion.content.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
    use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
    use tempfile::tempdir;

    fn test_adapter(temp_path: PathBuf) -> Arc<Adapter> {
        let monitor_config = MonitorConfig::with_timeout(Duration::from_secs(5));
        let monitor = Arc::new(OperationMonitor::new(monitor_config));
        let shell_pool_config = ShellPoolConfig::default();
        let shell_pool = Arc::new(ShellPoolManager::new(shell_pool_config));
        let sandbox = Arc::new(
            Sandbox::new(
                vec![temp_path],
                ahma_mcp::sandbox::SandboxMode::Test,
                false,
                false,
                false,
            )
            .unwrap(),
        );
        Arc::new(Adapter::new(monitor, shell_pool, sandbox).unwrap())
    }

    #[tokio::test]
    async fn test_validate_child_scopes() {
        let td = tempdir().unwrap();
        let adapter = test_adapter(td.path().to_path_buf());
        let config = TaskTreeConfig::default();
        let orchestrator = TaskTreeOrchestrator::new(config, adapter).unwrap();

        let parent_scopes = vec![td.path().to_path_buf()];

        // Subdir should pass
        let child_scopes = vec!["src".to_string()];
        let res = orchestrator.validate_child_scopes(&child_scopes, &parent_scopes);
        assert!(res.is_ok());
        assert_eq!(res.unwrap()[0], td.path().join("src"));

        // Escape should fail
        let escape_scopes = vec!["../outside".to_string()];
        let res = orchestrator.validate_child_scopes(&escape_scopes, &parent_scopes);
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_tool_prefix_matching() {
        let td = tempdir().unwrap();
        let adapter = test_adapter(td.path().to_path_buf());
        let config = TaskTreeConfig::default();
        let orchestrator = TaskTreeOrchestrator::new(config, adapter).unwrap();

        let mut tree = TaskTree::new();
        let root_id = tree.add_node(
            None,
            "root".to_string(),
            TaskType::Planning,
            None,
            Some(vec!["cargo".to_string(), "git".to_string()]),
            None,
        );

        let parent_tools = orchestrator.get_effective_parent_allowed_tools(&tree, root_id);
        assert_eq!(
            parent_tools,
            Some(vec!["cargo".to_string(), "git".to_string()])
        );

        // Check command prefix verification
        let allowed_tools = parent_tools.unwrap();
        let cmd = "cargo check";
        let is_allowed = allowed_tools
            .iter()
            .any(|prefix| cmd == prefix || cmd.starts_with(&format!("{} ", prefix)));
        assert!(is_allowed);

        let bad_cmd = "python script.py";
        let is_allowed_bad = allowed_tools
            .iter()
            .any(|prefix| bad_cmd == prefix || bad_cmd.starts_with(&format!("{} ", prefix)));
        assert!(!is_allowed_bad);
    }

    #[tokio::test]
    async fn test_build_branch_context() {
        let td = tempdir().unwrap();
        let adapter = test_adapter(td.path().to_path_buf());
        let config = TaskTreeConfig::default();
        let orchestrator = TaskTreeOrchestrator::new(config, adapter).unwrap();

        let mut tree = TaskTree::new();
        let root_id = tree.add_node(
            None,
            "root goal".to_string(),
            TaskType::Planning,
            None,
            None,
            None,
        );

        let step1_id = tree.add_node(
            Some(root_id),
            "step 1 description".to_string(),
            TaskType::ShellCommand {
                command: "cargo build".to_string(),
            },
            None,
            None,
            None,
        );

        let step2_id = tree.add_node(
            Some(root_id),
            "step 2 description".to_string(),
            TaskType::ShellCommand {
                command: "cargo test".to_string(),
            },
            None,
            None,
            None,
        );

        // Before step 1 completes, context of step 1 is empty
        let context1 = orchestrator.build_branch_context(&tree, step1_id);
        assert!(context1.contains("root goal"));
        assert!(!context1.contains("Completed Sibling"));

        // Set step 1 outcome (test shortcut: force the terminal state directly
        // rather than driving the full Created -> Running -> Completed path).
        if let Some(node) = tree.get_node_mut(step1_id) {
            node.state = crate::tree::NodeState::Completed;
            node.result = Some(NodeResult {
                exit_code: Some(0),
                stdout: "Build ok".to_string(),
                stderr: String::new(),
                summary: "Build succeeded".to_string(),
                success: true,
            });
        }

        // Context of step 2 should see completed step 1 sibling
        let context2 = orchestrator.build_branch_context(&tree, step2_id);
        assert!(context2.contains("step 1 description"));
        assert!(context2.contains("Build succeeded"));
    }
}
