# Project: GlideWin

## Goal

Build a Windows-native AI desktop companion inspired by Glide.

### Core Experience

Press **Ctrl+Shift+Space** → glidewin instantly captures your screen and starts listening. Speak your question or command. When you stop talking, the agent runs and speaks the answer back. Zero manual steps.

### MVP Features

* Global hotkey (Ctrl+Shift+Space) activates the widget
* Auto screenshot on activation (before window appears)
* Real-time voice transcription (streaming as user speaks)
* Silence detection triggers agent submission
* Screenshot + transcript fed to agentic loop
* Agent response spoken via TTS
* Widget mode: small floating overlay at top-center
* Fullscreen mode: see last 20 conversations

### Explicitly Out of Scope

* Accounts
* Billing
* Cloud backend
* OAuth
* Notion/Gmail integrations
* Multi-user support
* Cursor pointing
