# Server-side context editing (roadmap item 12) — design

Status: shipped 2026-11-07 (this document written before implementation,
per the house per-feature order).

## Problem

Loom's context story is client-side: the kernel's compilers project,
prune, and (as the last resort) hand off to an LLM compaction pass. The
Anthropic Messages API also offers *server-side* context editing
(`clear_tool_uses_20250919`, beta header
`context-management-2025-06-27`): past a token threshold the API itself
replaces old tool results with placeholders before the prompt reaches
the model, while the client keeps its full transcript. Roadmap item 12
lists provider-native context editing as a nicety; the offline-legal
slice is the request hint + adapter translation, default-off.

## Scope

- **In**: a provider-agnostic `ContextEditingHint` on
  `loom_kernel::model::ModelRequest`; Anthropic adapter translation
  into `context_management.edits[0] = clear_tool_uses_20250919` plus
  the beta header; `EngineConfig::context_editing` opt-in (default
  `None`) threaded through `AgentConfig` to the default compiler;
  wiremock tests proving the exact wire bytes on and the exact
  default-off absence.
- **Out (recorded, not built)**: OpenAI-compatible
  `compact_threshold` / `/responses/compact` (different mechanism —
  summary compaction, not selective clearing; needs its own hint
  semantics when a gateway proves it matters); `clear_thinking`
  (Claude 5.1+ never invalidates thinking with server-side edits, so
  the default already does the right thing); `compact_20260112`
  (server-side summary compaction — overlaps the kernel's own
  compaction authority and would double-manage context).

## Wire shape (pinned from platform.claude.com docs, 2026-11-07)

```json
{
  "context_management": {
    "edits": [
      {
        "type": "clear_tool_uses_20250919",
        "trigger":       {"type": "input_tokens", "value": 30000},
        "keep":          {"type": "tool_uses",    "value": 3},
        "clear_at_least": {"type": "input_tokens", "value": 5000},
        "exclude_tools": ["web_search"]
      }
    ]
  }
}
```

Headers: `anthropic-beta: context-management-2025-06-27` (only when a
context-management edit is sent). Semantics worth encoding in naming:
trigger fires on input tokens; `keep` counts recent tool uses kept;
`clear_at_least` amortizes the cache-prefix invalidation clearing
causes; `exclude_tools` spares named tools. The minimal valid form is
`{"type": "clear_tool_uses_20250919"}` alone (provider defaults), but
Loom always sends explicit `trigger`/`keep` so behavior is replayable
from the request bytes, not from unstated provider defaults.

## Design

### 1. Kernel: provider-agnostic hint (`loom-kernel/src/model.rs`)

```rust
pub struct ContextEditingHint {
    pub trigger_tokens: u32,          // {"type":"input_tokens"}
    pub keep_tool_uses: u32,          // {"type":"tool_uses"}
    pub clear_at_least_tokens: Option<u32>,
    pub exclude_tools: Vec<String>,
}
```

`ModelRequest` grows `context_editing: Option<ContextEditingHint>` with
`#[serde(default, skip_serializing_if = "Option::is_none")]`: additive,
and serialized requests are byte-identical while the hint is off —
same rule as every default-off flag. The kernel stays provider-blind:
the struct names *intent* ("clear stale tool results past a
threshold"), never provider wire names.

### 2. Compiler: copy-through (`context/default.rs`)

`AgentConfig` grows `context_editing: Option<ContextEditingHint>`
(default `None` via `AgentConfig::new`); the default compiler copies it
onto every `ModelRequest`. The compiler's own budget check is
unchanged — server-side editing reduces *provider-side* prompt size,
not the kernel's projection or its census, so no gate here can be
tuned by the hint.

### 3. Engine: opt-in (`loom-engine/src/engine.rs`)

`EngineConfig::context_editing: Option<ContextEditingHint>` (default
`None` in every constructor). Threaded where `AgentConfig` is built.
No CLI or eval flag ships in this slice: the eval protocol must stay
frozen, and a default-off knob no surface can set is dead code — the
opt-in is the library surface frontends use; a `--context-editing`
flag waits for an operator who wants it.

### 4. Anthropic adapter (`loom-providers`)

- `anthropic_wire.rs build_request_body`: when the hint is `Some`,
  add the `context_management` object exactly as pinned above
  (deterministic key order via `json!`).
- `anthropic.rs step()`: when the request body carries
  `context_management`, append the
  `anthropic-beta: context-management-2025-06-27` header. Detection is
  on the built body (single source of truth: the header rides the
  feature, it cannot drift from the body).
- Other adapters ignore the hint (it rides `ModelRequest`, which they
  already tolerate extras on).

## Tests (written first)

1. `loom-kernel`: hint round-trips through `ModelRequest` serde; a
   serialized request without the hint deserializes with `None`
   (backward compat).
2. `loom-kernel` compiler: `AgentConfig` with the hint produces a
   request carrying it; default config produces `None`.
3. `loom-providers` wiremock: with the hint, the recorded request body
   contains the exact `context_management` object and the request
   carries the beta header; without it, no `context_management` key
   and no beta header (byte-level default-off proof).

## Compatibility

- Default-off: every existing config, the frozen SWE-bench protocol,
  and the bare-loop baseline are untouched (no key on the wire, no
  header).
- Event schema: unchanged (the hint is on `ModelRequest`, not on
  events; no schema bump).
- Adapters: additive; non-Anthropic adapters see a new `Option` field
  they may ignore.

## Risks

- Server-side clearing invalidates the prompt-cache prefix when it
  fires — that is why `clear_at_least` exists and why the hint stays
  opt-in: an operator enabling it trades cache hits for context
  headroom on tool-heavy long tasks.
- Beta API surface: the strategy name and header are dated constants;
  if Anthropic renames, the adapter is the only file to touch.
