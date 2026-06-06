# Sharp - You Say, It Does

I got the inspiration from [clicky by Farza](https://x.com/heyclicky) and frustration with Microsoft Windows Copilot which can't really do stuff.

Sharp is your personal assistant which has access to the terminal, the web and can assist in mundane tasks.

## What it does

Press **Ctrl+Shift+Space** — Sharp captures your screen, listens to you speak, and acts. When you stop talking, the agent runs and speaks the answer back. No clicking, no typing.

- Sees your screen automatically on activation
- Transcribes your voice in real time
- Runs an agentic loop with access to your terminal and the web
- Speaks the response back via TTS
- Floats as a small overlay so it never gets in the way

## Stack

- **Frontend:** React + TypeScript + Vite
- **Backend:** Rust (Tauri v2)
- **AI:** OpenAI-compatible agentic loop with tool use

## Prerequisites

- [Node.js](https://nodejs.org/) (v18+)
- [Rust](https://rustup.rs/)
- [Tauri CLI prerequisites](https://tauri.app/start/prerequisites/) (Windows: Build Tools for Visual Studio)

## Getting Started

```bash
npm install
npm run tauri dev
```

To build a release binary:

```bash
npm run tauri build
```

## Shortcuts

- **Ctrl+Shift+Space** — toggle Sharp
- **Ctrl+Shift+H** — show history panel
- **Esc** — quick hide
