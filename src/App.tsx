import { useEffect, useRef, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import './App.css';

// ── Types ──────────────────────────────────────────────────────────────────

type ToolCallEntry = {
  tool: string;
  input: string;
  output?: string;
  status: 'running' | 'done' | 'error';
};

type ToolCallPayload = {
  tool: string;
  input: string;
  status: 'start' | 'done' | 'error';
  output?: string;
};

type WidgetState = 'idle' | 'listening' | 'transcribing' | 'thinking' | 'speaking' | 'done' | 'error';

const STATE_COLOR: Record<WidgetState, string> = {
  idle:        '#555555',
  listening:   '#ff4444',
  transcribing:'#7777ff',
  thinking:    '#f0b840',
  speaking:    '#44dd88',
  done:        '#44aa88',
  error:       '#ff6666',
};

// Normalized RMS threshold below which we consider it silence (0–1 scale).
const SILENCE_THRESHOLD = 0.02;
// How long silence must persist (ms) before auto-submitting.
const SILENCE_DURATION_MS = 1500;

// ── App ────────────────────────────────────────────────────────────────────

function App() {
  const [error, setError]                       = useState<string | null>(null);
  const [isRecording, setIsRecording]           = useState(false);
  const [elapsed, setElapsed]                   = useState(0);
  const [isTranscribing, setIsTranscribing]     = useState(false);
  const [transcript, setTranscript]             = useState<string | null>(null);
  const [isAgentThinking, setIsAgentThinking]   = useState(false);
  const [isSpeaking, setIsSpeaking]             = useState(false);
  const [lastResponse, setLastResponse]         = useState<string | null>(null);
  const [activeToolCalls, setActiveToolCalls]   = useState<ToolCallEntry[]>([]);
  const [hasScreenshot, setHasScreenshot]       = useState(false);
  const [silenceCountdown, setSilenceCountdown] = useState<number | null>(null);

  const pendingToolCallsRef  = useRef<ToolCallEntry[]>([]);
  const timerRef             = useRef<ReturnType<typeof setInterval> | null>(null);
  const screenshotRef        = useRef<string | null>(null);

  // Silence-detection refs — avoid stale closures in the audio-level listener.
  const isRecordingRef       = useRef(false);
  const hasSpokeRef          = useRef(false);       // true once first speech detected this session
  const silenceStartRef      = useRef<number | null>(null);
  const countdownIntervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const silenceCountdownRef  = useRef<number | null>(null); // mirror of state for keyboard handler

  // Always points to the latest doStopRecording so the countdown interval can call it.
  const doStopRef = useRef<(() => Promise<void>) | null>(null);

  useEffect(() => { isRecordingRef.current = isRecording; }, [isRecording]);

  // Derived widget state — order matters: higher priority first.
  const widgetState: WidgetState =
    error           ? 'error'        :
    isSpeaking      ? 'speaking'     :
    isAgentThinking ? 'thinking'     :
    isTranscribing  ? 'transcribing' :
    isRecording     ? 'listening'    :
    lastResponse    ? 'done'         :
    'idle';

  const fmt = (s: number) =>
    `${Math.floor(s / 60).toString().padStart(2, '0')}:${(s % 60).toString().padStart(2, '0')}`;

  // ── Actions ─────────────────────────────────────────────────────

  const dismissWindow = async () => {
    try { await getCurrentWindow().hide(); } catch { /* ignore */ }
  };

  const speakResponse = async (text: string) => {
    try {
      setIsSpeaking(true);
      await invoke('speak_text', { text });
    } catch (e: any) {
      setError('TTS: ' + e.toString());
    } finally {
      setIsSpeaking(false);
    }
  };

  const sendToAgent = async (msg: string) => {
    if (!msg.trim() || isAgentThinking) return;
    pendingToolCallsRef.current = [];
    setActiveToolCalls([]);
    try {
      setError(null);
      const response = await invoke<string>('agent_chat', {
        message: msg,
        screenshotPath: screenshotRef.current,
      });
      const toolCalls = [...pendingToolCallsRef.current];
      pendingToolCallsRef.current = [];
      setActiveToolCalls([]);
      setLastResponse(response);
      if (toolCalls.length > 0) console.debug('[glidewin] tool calls:', toolCalls);
      speakResponse(response);
    } catch (e: any) {
      pendingToolCallsRef.current = [];
      setActiveToolCalls([]);
      setError(e.toString());
    }
  };

  const clearCountdown = () => {
    if (countdownIntervalRef.current) { clearInterval(countdownIntervalRef.current); countdownIntervalRef.current = null; }
    silenceStartRef.current = null;
    setSilenceCountdown(null);
    silenceCountdownRef.current = null;
  };

  const doStopRecording = async () => {
    // Guard against double-invocation (e.g. Space pressed while countdown fires).
    if (!isRecordingRef.current) return;
    isRecordingRef.current = false;

    clearCountdown();
    hasSpokeRef.current = false;
    setIsRecording(false);
    setElapsed(0);
    if (timerRef.current) { clearInterval(timerRef.current); timerRef.current = null; }
    setIsTranscribing(true);
    try {
      const finalTranscript = await invoke<string>('stop_recording');
      setIsTranscribing(false);
      if (finalTranscript) {
        setTranscript(finalTranscript);
        await sendToAgent(finalTranscript);
      }
    } catch (e: any) {
      setIsTranscribing(false);
      setError(e.toString());
    }
  };

  // Keep doStopRef current so the countdown interval always calls the freshest version.
  useEffect(() => { doStopRef.current = doStopRecording; });

  // Cancel a pending auto-submit countdown; user must speak again to re-arm detection.
  const cancelCountdown = () => {
    clearCountdown();
    hasSpokeRef.current = false;
  };

  const toggleRecording = async () => {
    if (isRecording) {
      await doStopRecording();
    } else {
      setError(null);
      setTranscript(null);
      setLastResponse(null);
      hasSpokeRef.current = false;
      silenceStartRef.current = null;
      try {
        await invoke('start_recording');
        setIsRecording(true);
        isRecordingRef.current = true;
        timerRef.current = setInterval(() => setElapsed(p => p + 1), 1000);
      } catch (e: any) {
        setError(e.toString());
      }
    }
  };

  // ── Effects ──────────────────────────────────────────────────────

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<ToolCallPayload>('tool-call', event => {
      const { tool, input, status, output } = event.payload;
      if (status === 'start') {
        pendingToolCallsRef.current = [...pendingToolCallsRef.current, { tool, input, status: 'running' }];
      } else {
        const updated = [...pendingToolCallsRef.current];
        for (let i = updated.length - 1; i >= 0; i--) {
          if (updated[i].tool === tool && updated[i].status === 'running') {
            updated[i] = { ...updated[i], status: status === 'done' ? 'done' : 'error', output };
            break;
          }
        }
        pendingToolCallsRef.current = updated;
      }
      setActiveToolCalls([...pendingToolCallsRef.current]);
    }).then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<boolean>('agent-thinking', e => setIsAgentThinking(e.payload))
      .then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<string>('transcript-chunk', e => {
      setTranscript(prev => prev ? prev + ' ' + e.payload : e.payload);
    }).then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<string>('activate', event => {
      screenshotRef.current = event.payload || null;
      setHasScreenshot(!!event.payload);
    }).then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  // Silence detection: Rust emits normalized RMS every 100ms while recording.
  // We start a 1.5s countdown after the first silence post-speech; expiry auto-submits.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<number>('audio-level', e => {
      if (!isRecordingRef.current) return;
      const rms = e.payload;

      if (rms >= SILENCE_THRESHOLD) {
        // Speech detected — arm silence detection and clear any active countdown.
        hasSpokeRef.current = true;
        if (silenceStartRef.current !== null || countdownIntervalRef.current) {
          clearCountdown();
        }
      } else if (hasSpokeRef.current && silenceStartRef.current === null && !countdownIntervalRef.current) {
        // Silence began after speech — start the countdown.
        silenceStartRef.current = Date.now();
        const startMs = Date.now();
        countdownIntervalRef.current = setInterval(() => {
          const remaining = Math.max(0, SILENCE_DURATION_MS - (Date.now() - startMs));
          const secs = remaining / 1000;
          setSilenceCountdown(secs);
          silenceCountdownRef.current = secs;
          if (remaining <= 0) {
            clearInterval(countdownIntervalRef.current!);
            countdownIntervalRef.current = null;
            silenceStartRef.current = null;
            setSilenceCountdown(null);
            silenceCountdownRef.current = null;
            doStopRef.current?.();
          }
        }, 50);
      }
    }).then(fn => { unlisten = fn; });
    return () => {
      unlisten?.();
      if (countdownIntervalRef.current) { clearInterval(countdownIntervalRef.current); countdownIntervalRef.current = null; }
    };
  }, []);

  // Keyboard: Escape cancels countdown (or dismisses); Space toggles/force-submits.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (silenceCountdownRef.current !== null) { cancelCountdown(); return; }
        dismissWindow();
        return;
      }
      if (e.key === ' ' && !isTranscribing && !isAgentThinking && !isSpeaking) {
        e.preventDefault();
        toggleRecording();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [isRecording, isTranscribing, isAgentThinking, isSpeaking]);

  // ── Widget display helpers ─────────────────────────────────────

  const runningTool = activeToolCalls.find(t => t.status === 'running');

  const statusLabel =
    widgetState === 'idle'         ? 'Ready'                                      :
    widgetState === 'listening'    ? `Listening  ${fmt(elapsed)}`                 :
    widgetState === 'transcribing' ? 'Transcribing'                               :
    widgetState === 'thinking'     ? (runningTool ? runningTool.tool : 'Thinking') :
    widgetState === 'speaking'     ? 'Speaking'                                   :
    widgetState === 'done'         ? 'Done'                                       :
    'Error';

  const contentLine =
    (widgetState === 'listening' || widgetState === 'transcribing') ? (transcript ?? '') :
    widgetState === 'done'   ? (lastResponse ?? '') :
    widgetState === 'error'  ? (error ?? '')        :
    '';

  const hintLine =
    silenceCountdown !== null
      ? `Submitting in ${silenceCountdown.toFixed(1)}s  ·  Space to submit now  ·  Esc to cancel`
      : widgetState === 'idle' && hasScreenshot ? 'Screenshot ready  ·  Space to speak  ·  Esc to dismiss'
      : widgetState === 'idle'                  ? 'Space to record  ·  Esc to dismiss'
      : widgetState === 'listening'             ? 'Space to stop  ·  Esc to dismiss'
      : 'Esc to dismiss';

  const dotClass =
    widgetState === 'listening'                                   ? 'dot dot-pulse-red'   :
    widgetState === 'transcribing' || widgetState === 'thinking'  ? 'dot dot-pulse-amber' :
    widgetState === 'speaking'                                    ? 'dot dot-pulse-green' :
    'dot';

  const color = STATE_COLOR[widgetState];

  // ── Render ────────────────────────────────────────────────────

  return (
    <div className="widget" data-tauri-drag-region>
      <div className="widget-row" data-tauri-drag-region>
        <div className="status" style={{ color }}>
          <span className={dotClass} style={{ background: color }} />
          {statusLabel}
        </div>
        {contentLine && <div className="content">{contentLine}</div>}
        <button className="close-btn" onClick={dismissWindow} title="Dismiss (Esc)">✕</button>
      </div>
      <div className="hint" data-tauri-drag-region data-countdown={silenceCountdown !== null ? '' : undefined}>
        {hintLine}
      </div>
    </div>
  );
}

export default App;
