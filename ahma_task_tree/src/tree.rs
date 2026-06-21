use ahma_common::state_machine::{FsmState, InvalidTransition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a task node in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub usize);

/// The execution state of a task node (SPEC R23).
///
/// Transitions are guarded by [`NodeState::can_transition_to`] and applied
/// through the named methods on [`TaskNode`] (`start`, `complete`, `fail`,
/// `begin_backtracking`, `reset_for_retry`) rather than by assigning the field
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Created,
    Planning,
    Running,
    Completed,
    Failed,
    Backtracking,
}

impl NodeState {
    /// Whether `next` is a legal successor of `self`.
    ///
    /// The graph: a node is `Created`, runs (`Running`), and finishes
    /// `Completed` or `Failed`. A failed node may be retried (`-> Created`) or
    /// trigger recovery on its planning parent (`-> Backtracking`); a
    /// backtracking parent resumes (`-> Running`) once new children are planned.
    /// `Completed` is terminal.
    pub fn can_transition_to(self, next: NodeState) -> bool {
        use NodeState::*;
        matches!(
            (self, next),
            (Created, Running)
                | (Created, Planning)
                | (Planning, Running)
                | (Planning, Completed)
                | (Planning, Failed)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Backtracking)
                | (Failed, Created)
                | (Failed, Backtracking)
                | (Backtracking, Running)
                | (Backtracking, Created)
        )
    }
}

impl FsmState for NodeState {
    fn name(&self) -> &'static str {
        match self {
            NodeState::Created => "Created",
            NodeState::Planning => "Planning",
            NodeState::Running => "Running",
            NodeState::Completed => "Completed",
            NodeState::Failed => "Failed",
            NodeState::Backtracking => "Backtracking",
        }
    }

    fn is_terminal(&self) -> bool {
        // Completed is the only state with no legal successor. Failed is not
        // terminal: it may be retried or escalated to backtracking.
        matches!(self, NodeState::Completed)
    }
}

/// The type of operation a node performs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskType {
    /// A sandboxed shell command or predefined tool command.
    ShellCommand { command: String },
    /// An atomic LLM reasoning step.
    LlmCall { instructions: String },
    /// A decomposition node that splits the goal into subtasks.
    Planning,
}

/// The result returned by a completed or failed task node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    /// Exit code of the command (if shell tool).
    pub exit_code: Option<i32>,
    /// Raw stdout content.
    pub stdout: String,
    /// Raw stderr content.
    pub stderr: String,
    /// Concise summary of the outcome (verbatim if short, LLM-summarised if long).
    pub summary: String,
    /// Whether execution succeeded.
    pub success: bool,
}

/// A node in the hierarchical task tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub task_description: String,
    pub node_type: TaskType,
    pub state: NodeState,
    pub result: Option<NodeResult>,
    pub retry_count: usize,
    /// Narrowed filesystem scopes for this node (inherited/checked against parent).
    pub sandbox_scopes: Option<Vec<String>>,
    /// Restricted tool commands/prefixes for this node.
    pub allowed_tools: Option<Vec<String>>,
    /// Restricted egress domains allowed for this node.
    pub allowed_domains: Option<Vec<String>>,
}

impl TaskNode {
    /// Apply a guarded state transition (SPEC R23): advance to `next` only if it
    /// is a legal successor of the current state, otherwise return the rejected
    /// transition. This is the single entry point used by the named helpers
    /// below; the orchestrator never assigns `state` directly.
    fn transition_to(&mut self, next: NodeState) -> Result<(), InvalidTransition> {
        if self.state.can_transition_to(next) {
            self.state = next;
            Ok(())
        } else {
            Err(InvalidTransition {
                from: self.state.name(),
                action: next.name(),
            })
        }
    }

    /// Begin (or resume) execution: `-> Running`.
    pub fn start(&mut self) -> Result<(), InvalidTransition> {
        self.transition_to(NodeState::Running)
    }

    /// Record a successful outcome: `-> Completed`, storing `result`.
    pub fn complete(&mut self, result: NodeResult) -> Result<(), InvalidTransition> {
        self.transition_to(NodeState::Completed)?;
        self.result = Some(result);
        Ok(())
    }

    /// Record a failed outcome: `-> Failed`, storing `result`.
    pub fn fail(&mut self, result: NodeResult) -> Result<(), InvalidTransition> {
        self.transition_to(NodeState::Failed)?;
        self.result = Some(result);
        Ok(())
    }

    /// Escalate a planning node to recovery: `-> Backtracking`.
    pub fn begin_backtracking(&mut self) -> Result<(), InvalidTransition> {
        self.transition_to(NodeState::Backtracking)
    }

    /// Reset a failed node for another attempt: `-> Created`, recording the
    /// new retry counter.
    pub fn reset_for_retry(&mut self, retry_count: usize) -> Result<(), InvalidTransition> {
        self.transition_to(NodeState::Created)?;
        self.retry_count = retry_count;
        Ok(())
    }
}

/// A tree of tasks representing hierarchical decomposition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskTree {
    pub nodes: HashMap<NodeId, TaskNode>,
    pub root_id: Option<NodeId>,
    next_node_id: usize,
}

impl TaskTree {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            root_id: None,
            next_node_id: 0,
        }
    }

    /// Add a node to the tree.
    pub fn add_node(
        &mut self,
        parent_id: Option<NodeId>,
        task_description: String,
        node_type: TaskType,
        sandbox_scopes: Option<Vec<String>>,
        allowed_tools: Option<Vec<String>>,
        allowed_domains: Option<Vec<String>>,
    ) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id += 1;

        let node = TaskNode {
            id,
            parent_id,
            children: Vec::new(),
            task_description,
            node_type,
            state: NodeState::Created,
            result: None,
            retry_count: 0,
            sandbox_scopes,
            allowed_tools,
            allowed_domains,
        };

        self.nodes.insert(id, node);

        if let Some(parent) = parent_id {
            if let Some(parent_node) = self.nodes.get_mut(&parent) {
                parent_node.children.push(id);
            }
        } else if self.root_id.is_none() {
            self.root_id = Some(id);
        }

        id
    }

    /// Retrieve a node by reference.
    pub fn get_node(&self, id: NodeId) -> Option<&TaskNode> {
        self.nodes.get(&id)
    }

    /// Retrieve a node by mutable reference.
    pub fn get_node_mut(&mut self, id: NodeId) -> Option<&mut TaskNode> {
        self.nodes.get_mut(&id)
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    fn node() -> TaskNode {
        TaskNode {
            id: NodeId(0),
            parent_id: None,
            children: Vec::new(),
            task_description: String::new(),
            node_type: TaskType::Planning,
            state: NodeState::Created,
            result: None,
            retry_count: 0,
            sandbox_scopes: None,
            allowed_tools: None,
            allowed_domains: None,
        }
    }

    fn outcome(success: bool) -> NodeResult {
        NodeResult {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            summary: String::new(),
            success,
        }
    }

    #[test]
    fn happy_path_run_to_completion() {
        let mut n = node();
        n.start().unwrap();
        assert_eq!(n.state, NodeState::Running);
        n.complete(outcome(true)).unwrap();
        assert_eq!(n.state, NodeState::Completed);
        assert!(n.state.is_terminal());
        assert!(n.result.is_some());
    }

    #[test]
    fn failure_then_retry_resets_to_created() {
        let mut n = node();
        n.start().unwrap();
        n.fail(outcome(false)).unwrap();
        assert_eq!(n.state, NodeState::Failed);
        n.reset_for_retry(1).unwrap();
        assert_eq!(n.state, NodeState::Created);
        assert_eq!(n.retry_count, 1);
    }

    #[test]
    fn backtracking_cycle() {
        let mut n = node();
        n.start().unwrap();
        n.begin_backtracking().unwrap();
        assert_eq!(n.state, NodeState::Backtracking);
        n.start().unwrap(); // resume
        assert_eq!(n.state, NodeState::Running);
    }

    #[test]
    fn illegal_transitions_are_rejected_and_leave_state_unchanged() {
        // Completed is terminal: no transition leaves it.
        let mut n = node();
        n.start().unwrap();
        n.complete(outcome(true)).unwrap();
        let err = n.start().unwrap_err();
        assert_eq!(err.from, "Completed");
        assert_eq!(err.action, "Running");
        assert_eq!(n.state, NodeState::Completed);

        // Cannot complete straight from Created (must run first).
        let mut fresh = node();
        assert!(fresh.complete(outcome(true)).is_err());
        assert_eq!(fresh.state, NodeState::Created);
        assert!(fresh.result.is_none());
    }
}
