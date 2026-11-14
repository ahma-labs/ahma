// What crosses between the server and this page. Hand-written mirror of
// ahma_gui/src/dto.rs (protocol 1); that file is the source of truth.
//
// Rust `Option<T>` is `T | null` here (always present on the wire). A View's
// regions are *absent* when unchanged (`skip_serializing_if`), hence optional.

export const PROTOCOL = 1;

// ── Server → page ──

export interface View {
  protocol: number;
  rev: number;
  header?: Header;
  work?: Work;
  chat?: Chat;
  log?: Log;
  gates?: Gates;
}

export interface Header {
  workspace: string;
  hub: boolean;
  server: boolean;
  transport: string;
  sandbox: Sandbox;
  model: string | null;
  counts: Counts;
}

export interface Sandbox {
  label: string;
  enforced: boolean;
  sole_authority: boolean;
  disclosure: string | null;
  note: string | null;
  write: string[];
  read: string[];
}

export interface Counts {
  running: number;
  queued: number;
  succeeded: number;
  failed: number;
}

export interface Work {
  all_projects: boolean;
  project: string | null;
  sections: Section[];
}

export type SectionKind = "client" | "hooks" | "ended" | "local";

export interface Section {
  key: string;
  kind: SectionKind;
  title: string;
  scope: string;
  running: boolean;
  counts: Counts;
  latest: Latest | null;
  open: boolean;
  rows: Row[];
}

export interface Latest {
  title: string;
  status: string;
}

export interface Row {
  id: string;
  instance: string | null;
  title: string;
  status: string;
  exit_code: number | null;
  duration_ms: number | null;
  unsandboxed: boolean;
  denial: Denial | null;
  cancellable: boolean;
  expanded: boolean;
  output: string[];
}

export interface Denial {
  path: string;
  /** `read-only` or `read-write`. */
  access: string;
}

export interface Chat {
  entries: Entry[];
  turn: Turn | null;
  target: string | null;
}

export interface Turn {
  /** `loading`, `reading`, `thinking`, `writing`, `tool` or `awaiting-you`. */
  phase: string;
  tool: string | null;
}

export type Entry =
  | { kind: "user"; id: number; text: string }
  | { kind: "assistant"; id: number; blocks: Block[]; streaming: boolean }
  | { kind: "thinking"; id: number; text: string; streaming: boolean }
  | {
      kind: "tool";
      id: number;
      name: string;
      title: string;
      args: string;
      result: string | null;
      failed: boolean;
    }
  | { kind: "notice"; id: number; text: string };

export type Block =
  | { t: "para"; spans: Span[] }
  | { t: "heading"; level: number; spans: Span[] }
  | { t: "code"; lang: string | null; text: string }
  | { t: "list"; ordered: boolean; start: number | null; items: Block[][] }
  | { t: "quote"; blocks: Block[] }
  | { t: "rule" };

export type Span =
  | { t: "text"; s: string; em?: boolean; strong?: boolean }
  | { t: "code"; s: string }
  | { t: "link"; s: string; href: string };

export interface Log {
  lines: LogLine[];
  files: string[];
  active: string | null;
}

export interface LogLine {
  ts: string;
  /** `info`, `warn` or `error`. */
  level: string;
  text: string;
}

export interface Gates {
  waiting: number;
  top: Gate | null;
}

export type Gate =
  | {
      kind: "scope";
      decision_id: string;
      title: string;
      sections: GateSection[];
      options: GateOption[];
      advice: string | null;
    }
  | {
      kind: "web";
      decision_id: string;
      domain: string;
      url: string;
      tool: string | null;
      options: GateOption[];
    }
  | {
      kind: "tool";
      op_id: string;
      tool: string;
      description: string;
      note: string | null;
      options: GateOption[];
    };

export interface GateSection {
  heading: string;
  body: string;
}

export interface GateOption {
  value: string;
  label: string;
  granting: boolean;
  allowed: boolean;
  hint: string | null;
}

// ── Page → server ──

export type WireWebAnswer = "deny" | "once" | "session";
export type WireToolAnswer = "reject" | "once";

export type WireIntent =
  | { type: "submit"; text: string }
  | { type: "cancel_turn" }
  | { type: "answer_scope_grant"; decision_id: string; option: string }
  | { type: "toggle_scope_grant_detail"; decision_id: string }
  | { type: "answer_web_approval"; decision_id: string; answer: WireWebAnswer }
  | { type: "answer_tool_approval"; op_id: string; answer: WireToolAnswer }
  | { type: "cancel_op"; id: string; instance?: string | null }
  | { type: "re_raise_grant"; id: string; instance?: string | null }
  | { type: "open_section"; key: string | null }
  | { type: "chat_with"; key: string }
  | { type: "toggle_op_output"; id: string; instance?: string | null }
  | { type: "toggle_project_filter" };

export type WireReport =
  | { type: "gate_visible"; decision_id: string }
  | { type: "gate_hidden"; decision_id: string }
  | { type: "gate_save_line"; decision_id: string; visible: boolean };

/** Body of every non-2xx answer: `{"error":{"code","message"}}`. */
export interface ErrorBody {
  error: { code: string; message: string };
}
