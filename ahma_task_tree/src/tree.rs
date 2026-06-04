use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a task node in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub usize);

/// The execution state of a task node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Created,
    Planning,
    Running,
    Completed,
    Failed,
    Backtracking,
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
