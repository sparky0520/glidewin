# Tickets.md

## T0001

Project skeleton.
Done when:

- Tauri app launches
- React UI renders
- Build passes

## T0002

Global hotkey.
Done when:

- Ctrl+Space opens assistant
- Can close assistant

## T0003

Screenshot capture.
Done when:

- Button captures screen
- PNG saved locally

## T0004

Microphone recording.
Done when:

- Record button works
- WAV file generated

## T0005

Speech transcription.
Done when:

- WAV file transcribed
- Transcript displayed

## T0006

GPT integration.
Done when:

- Screenshot + transcript sent
- Response displayed

## T0007

Streaming responses.
Done when:

- Tokens appear incrementally

## T0008

Text-to-speech.
Done when:

- Response spoken aloud

## T0009

Agentic loop with PC control.
Done when:

- Last 20 conversations stored locally
- Agent can run PowerShell and open apps
- Tool calls shown in collapsible UI

## T0010

Widget mode UI.
Done when:

- Window is a small overlay (~480×120px) pinned to top-center of primary monitor
- Always-on-top, no taskbar entry
- Shows listening state, live transcript, agent status, and final answer in compact layout
- Escape or hotkey dismisses

## T0011

Real-time voice transcription.
Done when:

- Transcription streams token-by-token to UI while user is still speaking
- Replaces batch transcription (current: transcribe after stop)
- Uses Whisper streaming or equivalent (e.g. whisper.cpp with partial results)

## T0012

Silence detection and auto-submit.
Done when:

- Agent submission triggers automatically after N seconds of silence (default 1.5s)
- Visual countdown shown in widget before submit
- User can cancel or force-submit early

## T0013

Fullscreen history view.
Done when:

- Dedicated shortcut (e.g. Ctrl+Shift+H) or expand button opens fullscreen window
- Shows last 20 conversations: screenshot thumbnail, transcript, response, tool calls
- Widget and fullscreen can be open simultaneously
- Selecting a past conversation re-populates widget for follow-up
