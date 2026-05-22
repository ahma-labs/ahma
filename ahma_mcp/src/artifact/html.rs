//! HTML artifact template generation.
//!
//! Produces a self-contained single-file HTML artifact that:
//! - Embeds tool output data as a JSON blob.
//! - Renders a summary table / pre-formatted output.
//! - Provides an embedded LLM chat widget (vanilla JS, no external deps).
//! - Is fully offline-capable — the only external call is to the per-task
//!   localhost API server (or directly to Ollama on localhost).

use serde_json::Value;

// ─────────────────────────────────────────────────────────────────────────────
// ArtifactHtml
// ─────────────────────────────────────────────────────────────────────────────

/// A rendered HTML artifact string.
pub struct ArtifactHtml(String);

impl ArtifactHtml {
    /// Return the raw HTML as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Return the raw HTML as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Save the artifact to `path`.
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        std::fs::write(path, self.as_bytes())?;
        Ok(())
    }
}

impl From<ArtifactHtml> for String {
    fn from(a: ArtifactHtml) -> Self {
        a.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ArtifactBuilder
// ─────────────────────────────────────────────────────────────────────────────

/// Builds a self-contained HTML artifact.
#[derive(Default)]
pub struct ArtifactBuilder {
    title: String,
    description: Option<String>,
    data: Option<Value>,
    text_output: Option<String>,
    chat_endpoint: Option<String>,
    chat_model: Option<String>,
    api_token: Option<String>,
    /// URL of the per-task localhost API server (for chat relay).
    local_api_url: Option<String>,
}

impl ArtifactBuilder {
    /// Create a new builder with the given title.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    /// Human-readable description shown under the title.
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Structured data to embed and render (as a JSON table).
    pub fn data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Plain-text output to display in a `<pre>` block.
    pub fn text_output(mut self, text: impl Into<String>) -> Self {
        self.text_output = Some(text.into());
        self
    }

    /// OpenAI-compatible chat endpoint for the embedded chat widget.
    pub fn chat_endpoint(mut self, url: impl Into<String>) -> Self {
        self.chat_endpoint = Some(url.into());
        self
    }

    /// Model name used in chat requests.
    pub fn chat_model(mut self, model: impl Into<String>) -> Self {
        self.chat_model = Some(model.into());
        self
    }

    /// Bearer token for the chat endpoint.
    pub fn api_token(mut self, token: impl Into<String>) -> Self {
        self.api_token = Some(token.into());
        self
    }

    /// URL of the per-task artifact server relay (optional).
    pub fn local_api_url(mut self, url: impl Into<String>) -> Self {
        self.local_api_url = Some(url.into());
        self
    }

    /// Render and return the complete HTML artifact.
    pub fn build(self) -> ArtifactHtml {
        let html = render_template(
            &self.title,
            self.description.as_deref(),
            self.data.as_ref(),
            self.text_output.as_deref(),
            self.chat_endpoint
                .as_deref()
                .unwrap_or("http://localhost:11434/v1"),
            self.chat_model.as_deref().unwrap_or("llama3.2"),
            self.api_token.as_deref(),
            self.local_api_url.as_deref(),
        );
        ArtifactHtml(html)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Template renderer
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_template(
    title: &str,
    description: Option<&str>,
    data: Option<&Value>,
    text_output: Option<&str>,
    chat_endpoint: &str,
    chat_model: &str,
    api_token: Option<&str>,
    local_api_url: Option<&str>,
) -> String {
    let data_json = data
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "null".to_string());

    let text_block = text_output
        .map(|t| {
            format!(
                "<h2>Output</h2><pre class=\"output\">{}</pre>",
                html_escape(t)
            )
        })
        .unwrap_or_default();

    let desc_block = description
        .map(|d| format!("<p class=\"desc\">{}</p>", html_escape(d)))
        .unwrap_or_default();

    let relay_url = local_api_url.unwrap_or("");
    let token_str = api_token.unwrap_or("");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
  :root {{
    --bg: #0f1117; --fg: #e8eaed; --accent: #4f9cf9;
    --border: #2a2d36; --input-bg: #1a1d27;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ background: var(--bg); color: var(--fg); font-family: system-ui, sans-serif;
         padding: 1.5rem; max-width: 960px; margin: auto; }}
  h1 {{ font-size: 1.4rem; margin-bottom: .5rem; color: var(--accent); }}
  h2 {{ font-size: 1rem; margin: 1.2rem 0 .4rem; color: #aab; }}
  .desc {{ color: #aab; margin-bottom: 1rem; font-size: .9rem; }}
  pre.output {{ background: var(--input-bg); padding: 1rem; border-radius: 6px;
                overflow-x: auto; font-size: .85rem; border: 1px solid var(--border); }}
  #data-table {{ width: 100%; border-collapse: collapse; font-size: .85rem; }}
  #data-table th, #data-table td {{ padding: .4rem .8rem; border: 1px solid var(--border); text-align: left; }}
  #data-table th {{ background: var(--input-bg); }}
  #chat {{ margin-top: 1.5rem; }}
  #messages {{ background: var(--input-bg); border: 1px solid var(--border);
               border-radius: 6px; height: 260px; overflow-y: auto; padding: .8rem;
               font-size: .85rem; }}
  .msg {{ margin-bottom: .6rem; }}
  .msg.user {{ color: var(--accent); }}
  .msg.assistant {{ color: var(--fg); }}
  .msg.error {{ color: #f66; }}
  #input-row {{ display: flex; gap: .5rem; margin-top: .6rem; }}
  #user-input {{ flex: 1; background: var(--input-bg); border: 1px solid var(--border);
                 border-radius: 4px; color: var(--fg); padding: .5rem; font-size: .9rem; }}
  #send-btn {{ background: var(--accent); border: none; border-radius: 4px;
               color: #fff; padding: .5rem 1rem; cursor: pointer; font-size: .9rem; }}
  #send-btn:disabled {{ opacity: .5; cursor: default; }}
  .spinner {{ display: inline-block; width: 12px; height: 12px; border: 2px solid #4f9cf9;
              border-top-color: transparent; border-radius: 50%; animation: spin .7s linear infinite; }}
  @keyframes spin {{ to {{ transform: rotate(360deg); }} }}
</style>
</head>
<body>
<h1>{title}</h1>
{desc_block}
{text_block}

<div id="data-section"></div>

<div id="chat">
<h2>Ask about this data</h2>
<div id="messages"></div>
<div id="input-row">
  <input id="user-input" type="text" placeholder="Ask a question about this data…" autocomplete="off">
  <button id="send-btn">Send</button>
</div>
</div>

<script>
const RAW_DATA = {data_json};
const CHAT_ENDPOINT = "{chat_endpoint}";
const CHAT_MODEL = "{chat_model}";
const API_TOKEN = "{token_str}";
const RELAY_URL = "{relay_url}";

// ── Render data table ──────────────────────────────────────────────────────
(function renderData() {{
  const sec = document.getElementById('data-section');
  if (!RAW_DATA) return;
  if (typeof RAW_DATA === 'object' && !Array.isArray(RAW_DATA)) {{
    let html = '<h2>Data</h2><table id="data-table"><thead><tr><th>Key</th><th>Value</th></tr></thead><tbody>';
    for (const [k, v] of Object.entries(RAW_DATA)) {{
      html += `<tr><td>${{k}}</td><td>${{JSON.stringify(v)}}</td></tr>`;
    }}
    html += '</tbody></table>';
    sec.innerHTML = html;
  }} else {{
    sec.innerHTML = `<h2>Data</h2><pre class="output">${{JSON.stringify(RAW_DATA, null, 2)}}</pre>`;
  }}
}})();

// ── Chat widget ───────────────────────────────────────────────────────────
const messages = [];
const msgsEl = document.getElementById('messages');
const inputEl = document.getElementById('user-input');
const sendBtn = document.getElementById('send-btn');

function appendMsg(role, text) {{
  const div = document.createElement('div');
  div.className = `msg ${{role}}`;
  div.textContent = (role === 'user' ? 'You: ' : role === 'error' ? 'Error: ' : 'AI: ') + text;
  msgsEl.appendChild(div);
  msgsEl.scrollTop = msgsEl.scrollHeight;
}}

async function sendMessage() {{
  const text = inputEl.value.trim();
  if (!text) return;
  inputEl.value = '';
  sendBtn.disabled = true;

  appendMsg('user', text);
  messages.push({{ role: 'user', content: text }});

  // System context includes the embedded data.
  const systemContent = `You are an assistant helping the user understand the following data artifact.\nTitle: {title}\nData: ${{JSON.stringify(RAW_DATA)}}`;

  const payload = {{
    model: CHAT_MODEL,
    messages: [{{ role: 'system', content: systemContent }}, ...messages],
    stream: false,
  }};

  const headers = {{ 'Content-Type': 'application/json' }};
  if (API_TOKEN) headers['Authorization'] = 'Bearer ' + API_TOKEN;

  const endpoint = RELAY_URL ? `${{RELAY_URL}}/chat` : `${{CHAT_ENDPOINT}}/chat/completions`;

  const spinner = document.createElement('div');
  spinner.className = 'spinner';
  msgsEl.appendChild(spinner);

  try {{
    const resp = await fetch(endpoint, {{ method: 'POST', headers, body: JSON.stringify(payload) }});
    spinner.remove();
    if (!resp.ok) throw new Error(`HTTP ${{resp.status}}: ${{await resp.text()}}`);
    const json = await resp.json();
    const reply = json.choices?.[0]?.message?.content ?? '(no response)';
    messages.push({{ role: 'assistant', content: reply }});
    appendMsg('assistant', reply);
  }} catch (e) {{
    spinner.remove();
    appendMsg('error', e.message);
  }} finally {{
    sendBtn.disabled = false;
    inputEl.focus();
  }}
}}

sendBtn.addEventListener('click', sendMessage);
inputEl.addEventListener('keydown', e => {{ if (e.key === 'Enter') sendMessage(); }});
inputEl.focus();
</script>
</body>
</html>"#
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_produces_valid_html() {
        let html = ArtifactBuilder::new("Test Report")
            .description("Q4 summary")
            .data(json!({"revenue": 1000, "growth": "10%"}))
            .chat_model("llama3.2")
            .build();

        let s = html.as_str();
        assert!(s.contains("<!DOCTYPE html>"));
        assert!(s.contains("Test Report"));
        assert!(s.contains("Q4 summary"));
        assert!(s.contains("1000"));
        assert!(s.contains("llama3.2"));
    }

    #[test]
    fn build_with_text_output() {
        let html = ArtifactBuilder::new("Build Log")
            .text_output("Compiling... Done in 3.2s")
            .build();
        assert!(html.as_str().contains("Compiling"));
    }

    #[test]
    fn html_escape_works() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
    }
}
