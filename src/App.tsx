import { useEffect, useRef, useState } from 'react';
import { register, unregisterAll } from '@tauri-apps/plugin-global-shortcut';
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

type ChatMessage = {
  role: 'user' | 'assistant';
  text: string;
  toolCalls?: ToolCallEntry[];
};

type ToolCallPayload = {
  tool: string;
  input: string;
  status: 'start' | 'done' | 'error';
  output?: string;
};

// ── ToolCallsBlock ─────────────────────────────────────────────────────────

function ToolCallsBlock({ toolCalls, live = false }: { toolCalls: ToolCallEntry[]; live?: boolean }) {
  if (toolCalls.length === 0) return null;

  const hasRunning = toolCalls.some(t => t.status === 'running');
  const label = hasRunning
    ? 'Running tools…'
    : `${toolCalls.length} tool call${toolCalls.length !== 1 ? 's' : ''}`;

  const statusIcon = (s: ToolCallEntry['status']) =>
    s === 'done' ? '✓' : s === 'error' ? '✗' : '⋯';
  const statusColor = (s: ToolCallEntry['status']) =>
    s === 'done' ? '#7fff7f' : s === 'error' ? '#ff7f7f' : '#f0c060';

  return (
    <details
      open={live}
      style={{ marginBottom: '0.5rem', fontSize: '0.82rem' }}
    >
      <summary style={{
        cursor: 'pointer',
        color: '#888',
        listStyle: 'none',
        display: 'flex',
        alignItems: 'center',
        gap: '0.4rem',
        padding: '4px 0',
        userSelect: 'none',
      }}>
        <span style={{ fontSize: '0.7rem', color: hasRunning ? '#f0c060' : '#555' }}>▶</span>
        <span>{label}</span>
      </summary>

      <div style={{ marginTop: '0.4rem', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
        {toolCalls.map((tc, i) => (
          <div
            key={i}
            style={{
              borderLeft: `2px solid ${statusColor(tc.status)}`,
              paddingLeft: '0.6rem',
            }}
          >
            {/* Tool header */}
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', marginBottom: '0.25rem' }}>
              <span style={{ color: statusColor(tc.status), fontSize: '0.75rem', fontWeight: 'bold' }}>
                {statusIcon(tc.status)}
              </span>
              <code style={{ color: '#aaa', fontSize: '0.8rem' }}>{tc.tool}</code>
              {tc.status === 'running' && (
                <span className="transcribing-dot" style={{ marginLeft: '0.2rem' }} />
              )}
            </div>

            {/* Input */}
            <details style={{ marginBottom: '0.2rem' }}>
              <summary style={{ cursor: 'pointer', color: '#666', fontSize: '0.75rem', userSelect: 'none' }}>
                Input
              </summary>
              <pre style={{
                margin: '0.25rem 0 0',
                padding: '0.4rem 0.6rem',
                background: '#111',
                borderRadius: 4,
                color: '#ccc',
                fontSize: '0.75rem',
                overflowX: 'auto',
                maxHeight: 140,
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
              }}>
                {tc.input}
              </pre>
            </details>

            {/* Output */}
            {tc.output && (
              <details>
                <summary style={{ cursor: 'pointer', color: '#666', fontSize: '0.75rem', userSelect: 'none' }}>
                  Output
                </summary>
                <pre style={{
                  margin: '0.25rem 0 0',
                  padding: '0.4rem 0.6rem',
                  background: '#111',
                  borderRadius: 4,
                  color: tc.status === 'error' ? '#ff9999' : '#9f9',
                  fontSize: '0.75rem',
                  overflowX: 'auto',
                  maxHeight: 200,
                  whiteSpace: 'pre-wrap',
                  wordBreak: 'break-all',
                }}>
                  {tc.output}
                </pre>
              </details>
            )}
          </div>
        ))}
      </div>
    </details>
  );
}

// ── App ────────────────────────────────────────────────────────────────────

function App() {
  const [error, setError] = useState<string | null>(null);

  // Visual mode
  const [screenshotPath, setScreenshotPath] = useState<string | null>(null);
  const [isCapturing, setIsCapturing] = useState(false);
  const [isAsking, setIsAsking] = useState(false);
  const [gptResponse, setGptResponse] = useState<string | null>(null);
  const [isSpeaking, setIsSpeaking] = useState(false);

  // Recording
  const [isRecording, setIsRecording] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [transcript, setTranscript] = useState<string | null>(null);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // Agent mode
  const [agentInput, setAgentInput] = useState('');
  const [isAgentThinking, setIsAgentThinking] = useState(false);
  const [conversation, setConversation] = useState<ChatMessage[]>([]);
  const [activeToolCalls, setActiveToolCalls] = useState<ToolCallEntry[]>([]);
  const pendingToolCallsRef = useRef<ToolCallEntry[]>([]);
  const chatEndRef = useRef<HTMLDivElement>(null);

  // ── Window helpers ────────────────────────────────────────────────────

  const dismissWindow = async () => {
    try { await getCurrentWindow().hide(); }
    catch (e: any) { setError(e.toString()); }
  };

  // ── Screenshot ───────────────────────────────────────────────────────

  const takeScreenshot = async () => {
    try {
      setError(null);
      setIsCapturing(true);
      setScreenshotPath(null);
      const win = getCurrentWindow();
      await win.hide();
      await new Promise(r => setTimeout(r, 200));
      const path = await invoke<string>('capture_screen');
      setScreenshotPath(path);
      await win.show();
      await win.setFocus();
    } catch (e: any) {
      setError('Capture failed: ' + e.toString());
      await getCurrentWindow().show().catch(console.error);
    } finally {
      setIsCapturing(false);
    }
  };

  // ── TTS ──────────────────────────────────────────────────────────────

  const speakResponse = async (text: string) => {
    try {
      setError(null);
      setIsSpeaking(true);
      await invoke('speak_text', { text });
    } catch (e: any) {
      setError('TTS failed: ' + e.toString());
    } finally {
      setIsSpeaking(false);
    }
  };

  // ── Visual mode ──────────────────────────────────────────────────────

  const askGpt = async () => {
    if (!screenshotPath || !transcript) return;
    setError(null);
    setIsAsking(true);
    setGptResponse(null);
    const tokenUnlisten = await listen<string>('gpt-token', e => {
      setGptResponse(prev => (prev ?? '') + e.payload);
    });
    try {
      await invoke('ask_gpt_stream', { screenshotPath, transcript });
    } catch (e: any) {
      setError('GPT request failed: ' + e.toString());
    } finally {
      tokenUnlisten();
      setIsAsking(false);
    }
  };

  // ── Recording ────────────────────────────────────────────────────────

  const transcribeFile = async (filePath: string) => {
    try {
      setIsTranscribing(true);
      setTranscript(null);
      const text = await invoke<string>('transcribe_audio', { filePath });
      setTranscript(text);
      setAgentInput(text);
    } catch (e: any) {
      setError('Transcription failed: ' + e.toString());
    } finally {
      setIsTranscribing(false);
    }
  };

  const toggleRecording = async () => {
    if (isRecording) {
      try {
        setError(null);
        const path = await invoke<string>('stop_recording');
        transcribeFile(path);
      } catch (e: any) {
        setError('Stop recording failed: ' + e.toString());
      } finally {
        setIsRecording(false);
        setElapsed(0);
        if (timerRef.current) { clearInterval(timerRef.current); timerRef.current = null; }
      }
    } else {
      try {
        setError(null);
        setTranscript(null);
        setGptResponse(null);
        await invoke<string>('start_recording');
        setIsRecording(true);
        setElapsed(0);
        timerRef.current = setInterval(() => setElapsed(p => p + 1), 1000);
      } catch (e: any) {
        setError('Start recording failed: ' + e.toString());
      }
    }
  };

  // ── Agent mode ───────────────────────────────────────────────────────

  const sendAgentMessage = async () => {
    const msg = agentInput.trim();
    if (!msg || isAgentThinking) return;

    setAgentInput('');
    setConversation(prev => [...prev, { role: 'user', text: msg }]);
    pendingToolCallsRef.current = [];
    setActiveToolCalls([]);

    try {
      setError(null);
      const response = await invoke<string>('agent_chat', {
        message: msg,
        screenshotPath: screenshotPath ?? null,
      });
      const toolCalls = [...pendingToolCallsRef.current];
      pendingToolCallsRef.current = [];
      setActiveToolCalls([]);
      setConversation(prev => [...prev, { role: 'assistant', text: response, toolCalls }]);
    } catch (e: any) {
      pendingToolCallsRef.current = [];
      setActiveToolCalls([]);
      setError('Agent error: ' + e.toString());
    }
  };

  const clearConversation = async () => {
    await invoke('clear_conversation').catch(console.error);
    setConversation([]);
    setActiveToolCalls([]);
    pendingToolCallsRef.current = [];
    setScreenshotPath(null);
    setTranscript(null);
    setGptResponse(null);
    setAgentInput('');
    setError(null);
  };

  const formatTime = (s: number) =>
    `${Math.floor(s / 60).toString().padStart(2, '0')}:${(s % 60).toString().padStart(2, '0')}`;

  // ── Effects ──────────────────────────────────────────────────────────

  // tool-call events from Rust
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<ToolCallPayload>('tool-call', event => {
      const { tool, input, status, output } = event.payload;

      if (status === 'start') {
        pendingToolCallsRef.current = [
          ...pendingToolCallsRef.current,
          { tool, input, status: 'running' },
        ];
      } else {
        const updated = [...pendingToolCallsRef.current];
        // Find the last running entry for this tool and update it
        for (let i = updated.length - 1; i >= 0; i--) {
          if (updated[i].tool === tool && updated[i].status === 'running') {
            updated[i] = {
              ...updated[i],
              status: status === 'done' ? 'done' : 'error',
              output,
            };
            break;
          }
        }
        pendingToolCallsRef.current = updated;
      }
      setActiveToolCalls([...pendingToolCallsRef.current]);
    }).then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  // agent-thinking events
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<boolean>('agent-thinking', e => setIsAgentThinking(e.payload))
      .then(fn => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  // Scroll to bottom on new messages
  useEffect(() => {
    chatEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [conversation, activeToolCalls, isAgentThinking]);

  // Global shortcut
  useEffect(() => {
    (async () => {
      try {
        await unregisterAll();
        await register('CommandOrControl+Shift+Space', async event => {
          if (event.state !== 'Pressed') return;
          const win = getCurrentWindow();
          if (await win.isVisible()) { await win.hide(); }
          else { await win.show(); await win.setFocus(); }
        });
      } catch (err: any) {
        setError('Failed to register shortcut: ' + err.toString());
      }
    })();
    return () => { unregisterAll().catch(console.error); };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') dismissWindow(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // ── Render ───────────────────────────────────────────────────────────

  return (
    <div className="container">
      <h1>GlideWin</h1>

      {/* Controls */}
      <div style={{ display: 'flex', gap: '0.5rem', justifyContent: 'center', flexWrap: 'wrap', margin: '0.75rem 0' }}>
        <button onClick={takeScreenshot} disabled={isCapturing}>
          {isCapturing ? 'Capturing…' : screenshotPath ? 'Recapture' : 'Capture Screen'}
        </button>
        <button className={isRecording ? 'recording-btn' : ''} onClick={toggleRecording}>
          {isRecording ? <><span className="recording-dot" />Stop {formatTime(elapsed)}</> : 'Record'}
        </button>
        <button onClick={askGpt} disabled={!screenshotPath || !transcript || isAsking}>
          {isAsking ? 'Asking…' : 'Ask GPT (Visual)'}
        </button>
        <button onClick={clearConversation}>Clear</button>
        <button onClick={dismissWindow}>Dismiss</button>
      </div>

      {/* Status pills */}
      <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap', justifyContent: 'center', marginBottom: '0.5rem' }}>
        {screenshotPath && (
          <span style={{ fontSize: '0.75rem', background: '#2a4a2a', color: '#7fff7f', padding: '2px 8px', borderRadius: 12 }}>
            Screenshot attached
          </span>
        )}
        {isTranscribing && (
          <span style={{ fontSize: '0.75rem', background: '#1a1a3a', color: '#7f7fff', padding: '2px 8px', borderRadius: 12 }}>
            <span className="transcribing-dot" /> Transcribing…
          </span>
        )}
        {transcript && !isTranscribing && (
          <span style={{ fontSize: '0.75rem', background: '#2a2a2a', color: '#aaa', padding: '2px 8px', borderRadius: 12 }}>
            Voice: &ldquo;{transcript.slice(0, 55)}{transcript.length > 55 ? '…' : ''}&rdquo;
          </span>
        )}
      </div>

      {/* Visual mode response */}
      {gptResponse && (
        <div style={{ margin: '0.5rem 0', padding: '0.75rem', background: '#1a1a2a', borderRadius: 8, color: '#c8c8ff', textAlign: 'left' }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '0.25rem' }}>
            <strong style={{ fontSize: '0.8rem', color: '#888' }}>GPT (visual)</strong>
            <button onClick={() => speakResponse(gptResponse)} disabled={isSpeaking} style={{ fontSize: '0.75rem', padding: '2px 8px' }}>
              {isSpeaking ? 'Speaking…' : '🔊 Speak'}
            </button>
          </div>
          <p style={{ margin: 0, lineHeight: 1.6, whiteSpace: 'pre-wrap' }}>{gptResponse}</p>
        </div>
      )}

      {/* Conversation */}
      {(conversation.length > 0 || isAgentThinking) && (
        <div style={{ maxHeight: 380, overflowY: 'auto', margin: '0.5rem 0', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
          {conversation.map((msg, i) => (
            <div key={i} style={{
              padding: '0.6rem 0.75rem',
              borderRadius: 8,
              textAlign: 'left',
              background: msg.role === 'user' ? '#2a2a2a' : '#131a13',
              alignSelf: msg.role === 'user' ? 'flex-end' : 'flex-start',
              maxWidth: '92%',
            }}>
              <div style={{ fontSize: '0.7rem', color: '#666', marginBottom: '0.25rem' }}>
                {msg.role === 'user' ? 'You' : 'GlideWin'}
              </div>

              {/* Tool calls for this message */}
              {msg.role === 'assistant' && msg.toolCalls && msg.toolCalls.length > 0 && (
                <ToolCallsBlock toolCalls={msg.toolCalls} />
              )}

              <div style={{ color: msg.role === 'user' ? '#ddd' : '#7fff7f', whiteSpace: 'pre-wrap', lineHeight: 1.55 }}>
                {msg.text}
              </div>

              {msg.role === 'assistant' && (
                <button
                  onClick={() => speakResponse(msg.text)}
                  disabled={isSpeaking}
                  style={{ fontSize: '0.7rem', padding: '2px 6px', marginTop: '0.35rem' }}
                >
                  {isSpeaking ? '…' : '🔊'}
                </button>
              )}
            </div>
          ))}

          {/* Live thinking / tool call display */}
          {isAgentThinking && (
            <div style={{
              padding: '0.6rem 0.75rem',
              borderRadius: 8,
              background: '#131a13',
              alignSelf: 'flex-start',
              maxWidth: '92%',
            }}>
              <div style={{ fontSize: '0.7rem', color: '#666', marginBottom: '0.25rem' }}>GlideWin</div>
              {activeToolCalls.length > 0
                ? <ToolCallsBlock toolCalls={activeToolCalls} live />
                : <span style={{ color: '#7fff7f', fontSize: '0.85rem' }}>
                    <span className="transcribing-dot" /> Thinking…
                  </span>
              }
            </div>
          )}

          <div ref={chatEndRef} />
        </div>
      )}

      {/* Agent input */}
      <div style={{ display: 'flex', gap: '0.5rem', marginTop: '0.5rem' }}>
        <input
          type="text"
          value={agentInput}
          onChange={e => setAgentInput(e.target.value)}
          onKeyDown={e => e.key === 'Enter' && sendAgentMessage()}
          placeholder={screenshotPath ? 'Ask about screen or give a command…' : 'Give a command or ask anything…'}
          disabled={isAgentThinking}
          style={{
            flex: 1,
            padding: '0.5rem 0.75rem',
            borderRadius: 8,
            border: '1px solid #555',
            background: '#1a1a1a',
            color: '#fff',
            fontSize: '0.9rem',
          }}
        />
        <button
          onClick={sendAgentMessage}
          disabled={!agentInput.trim() || isAgentThinking}
          style={{ padding: '0.5rem 1rem', fontWeight: 'bold' }}
        >
          {isAgentThinking ? '…' : 'Send'}
        </button>
      </div>
      <div style={{ fontSize: '0.7rem', color: '#555', marginTop: '0.25rem', textAlign: 'left' }}>
        Tools: run_powershell · open_app{screenshotPath ? ' · screenshot context' : ''}
      </div>

      {error && (
        <div style={{ color: 'red', marginTop: '0.75rem' }}>
          <strong>Error:</strong> {error}
        </div>
      )}
    </div>
  );
}

export default App;
