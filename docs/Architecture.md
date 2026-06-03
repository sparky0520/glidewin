# GlideWin Architecture

## Activation Flow

User presses Ctrl+Shift+Space
    ↓
Widget window appears (small overlay, top-center, always-on-top)
    ↓
Screenshot captured automatically (window hides briefly → captures → re-shows)
    ↓
Microphone opens, real-time transcription begins (streaming tokens to UI)
    ↓
User speaks → transcript builds live in widget
    ↓
Silence detected (configurable threshold) → recording stops
    ↓
Screenshot + full transcript sent to agentic loop
    ↓
Agent runs (tools, multi-turn internally)
    ↓
Final response displayed in widget + spoken via TTS
    ↓
Widget stays visible; press hotkey again or Escape to dismiss

## Window Modes

**Widget mode** (default)

- Small overlay (~480×120px) pinned to top-center of primary monitor
- Shows: live transcription while listening, agent status while running, final answer
- Always-on-top, click-through background

**Fullscreen mode** (toggle or dedicated shortcut)

- Expands to show full conversation history (last 20 stored locally)
- Each entry: screenshot thumbnail, transcript, agent response, tool calls (collapsible)
- Keyboard nav, search/filter conversations

## Data Flow

```text
hotkey press
  → [Rust] hide window, capture_screen → PNG path
  → [Rust] start_realtime_transcription → streams tokens via "transcript-token" event
  → [React] accumulates transcript in UI
  → silence detected → stop_realtime_transcription → final transcript string
  → [Rust] agent_chat(screenshot_path, transcript) → streams via "tool-call" + "agent-token" events
  → [Rust] speak_text(final_response)
  → [Rust] save_conversation(entry) → local JSON store (last 20)
```

## Storage

Conversations stored in `{app_data}/glidewin/history.json` — array of last 20 entries, each:

```json
{
  "id": "uuid",
  "timestamp": "ISO8601",
  "screenshot_path": "...",
  "transcript": "...",
  "response": "...",
  "tool_calls": []
}
```
