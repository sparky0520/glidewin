import { useEffect, useState, useRef } from 'react';
import { register, unregisterAll } from '@tauri-apps/plugin-global-shortcut';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import './App.css';

function App() {
  const [error, setError] = useState<string | null>(null);
  const [screenshotPath, setScreenshotPath] = useState<string | null>(null);
  const [isCapturing, setIsCapturing] = useState(false);
  const [isRecording, setIsRecording] = useState(false);
  const [recordingPath, setRecordingPath] = useState<string | null>(null);
  const [elapsed, setElapsed] = useState(0);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [transcript, setTranscript] = useState<string | null>(null);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const dismissWindow = async () => {
    try {
      const appWindow = getCurrentWindow();
      await appWindow.hide();
    } catch (e: any) {
      setError(e.toString());
    }
  };

  const takeScreenshot = async () => {
    try {
      setError(null);
      setIsCapturing(true);
      setScreenshotPath(null);

      const appWindow = getCurrentWindow();
      await appWindow.hide();

      await new Promise(resolve => setTimeout(resolve, 200));

      const path = await invoke<string>('capture_screen');
      setScreenshotPath(path);

      await appWindow.show();
      await appWindow.setFocus();
    } catch (e: any) {
      setError('Capture failed: ' + e.toString());
      const appWindow = getCurrentWindow();
      await appWindow.show().catch(console.error);
    } finally {
      setIsCapturing(false);
    }
  };

  const transcribeFile = async (filePath: string) => {
    try {
      setIsTranscribing(true);
      setTranscript(null);
      const text = await invoke<string>('transcribe_audio', { filePath });
      setTranscript(text);
    } catch (e: any) {
      setError('Transcription failed: ' + e.toString());
    } finally {
      setIsTranscribing(false);
    }
  };

  const toggleRecording = async () => {
    if (isRecording) {
      // Stop recording
      try {
        setError(null);
        const path = await invoke<string>('stop_recording');
        setRecordingPath(path);
        // Auto-transcribe
        transcribeFile(path);
      } catch (e: any) {
        setError('Stop recording failed: ' + e.toString());
      } finally {
        setIsRecording(false);
        setElapsed(0);
        if (timerRef.current) {
          clearInterval(timerRef.current);
          timerRef.current = null;
        }
      }
    } else {
      // Start recording
      try {
        setError(null);
        setRecordingPath(null);
        setTranscript(null);
        await invoke<string>('start_recording');
        setIsRecording(true);
        setElapsed(0);
        timerRef.current = setInterval(() => {
          setElapsed(prev => prev + 1);
        }, 1000);
      } catch (e: any) {
        setError('Start recording failed: ' + e.toString());
      }
    }
  };

  const formatTime = (seconds: number): string => {
    const m = Math.floor(seconds / 60).toString().padStart(2, '0');
    const s = (seconds % 60).toString().padStart(2, '0');
    return `${m}:${s}`;
  };

  useEffect(() => {
    const setupShortcut = async () => {
      try {
        await unregisterAll();
        await register('CommandOrControl+Shift+Space', async (event) => {
          if (event.state === 'Pressed') {
            try {
              const appWindow = getCurrentWindow();
              const isVisible = await appWindow.isVisible();
              if (isVisible) {
                await appWindow.hide();
              } else {
                await appWindow.show();
                await appWindow.setFocus();
              }
            } catch (e: any) {
              setError('Shortcut handler error: ' + e.toString());
            }
          }
        });
      } catch (err: any) {
        setError('Failed to register shortcut: ' + err.toString());
      }
    };

    setupShortcut();

    return () => {
      unregisterAll().catch(e => console.error(e));
      if (timerRef.current) {
        clearInterval(timerRef.current);
      }
    };
  }, []);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        dismissWindow();
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, []);

  return (
    <div className="container">
      <h1>GlideWin Assistant</h1>
      <p>I am your desktop AI companion.</p>
      <p>Press <code>Ctrl+Shift+Space</code> globally to toggle this window.</p>

      <div style={{ margin: '1rem 0', display: 'flex', gap: '1rem', justifyContent: 'center', flexWrap: 'wrap' }}>
        <button onClick={takeScreenshot} disabled={isCapturing}>
          {isCapturing ? 'Capturing...' : 'Capture Screen'}
        </button>
        <button
          className={isRecording ? 'recording-btn' : ''}
          onClick={toggleRecording}
        >
          {isRecording ? (
            <>
              <span className="recording-dot" />
              Stop {formatTime(elapsed)}
            </>
          ) : (
            'Record'
          )}
        </button>
        <button onClick={dismissWindow}>Dismiss (Esc)</button>
      </div>

      {screenshotPath && (
        <div style={{ marginTop: '1rem', padding: '1rem', backgroundColor: '#333', borderRadius: '8px', color: 'white' }}>
          <strong>Screenshot saved to:</strong><br />
          <code style={{ wordBreak: 'break-all' }}>{screenshotPath}</code>
        </div>
      )}

      {recordingPath && (
        <div style={{ marginTop: '1rem', padding: '1rem', backgroundColor: '#1a3a1a', borderRadius: '8px', color: '#7fff7f' }}>
          <strong>Recording saved to:</strong><br />
          <code style={{ wordBreak: 'break-all' }}>{recordingPath}</code>
        </div>
      )}

      {isTranscribing && (
        <div style={{ marginTop: '1rem', padding: '1rem', backgroundColor: '#1a1a3a', borderRadius: '8px', color: '#7f7fff' }}>
          <span className="transcribing-dot" /> Transcribing...
        </div>
      )}

      {transcript && (
        <div style={{ marginTop: '1rem', padding: '1rem', backgroundColor: '#2a2a2a', borderRadius: '8px', color: 'white', textAlign: 'left' }}>
          <strong>Transcript:</strong>
          <p style={{ margin: '0.5rem 0 0', lineHeight: '1.6' }}>{transcript}</p>
        </div>
      )}

      {error && <div style={{ color: 'red', marginTop: '1rem' }}><strong>Error:</strong> {error}</div>}
    </div>
  );
}

export default App;
