import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

// ── Types ──────────────────────────────────────────────────────────────────

type ToolCallRecord = {
  tool: string;
  input: string;
  output?: string | null;
  status: string;
};

type ConversationRecord = {
  id: string;
  timestamp: number;
  transcript: string;
  response: string;
  screenshotPath?: string | null;
  toolCalls: ToolCallRecord[];
};

// ── HistoryView ────────────────────────────────────────────────────────────

function HistoryView() {
  const [records, setRecords]             = useState<ConversationRecord[]>([]);
  const [selectedId, setSelectedId]       = useState<string | null>(null);
  const [screenshots, setScreenshots]     = useState<Record<string, string>>({});
  const [loadingShot, setLoadingShot]     = useState<string | null>(null);

  useEffect(() => {
    invoke<ConversationRecord[]>('get_history').then(setRecords);
  }, []);

  const selected = records.find(r => r.id === selectedId) ?? null;

  useEffect(() => {
    if (!selected?.screenshotPath) return;
    const path = selected.screenshotPath;
    if (screenshots[path] || loadingShot === path) return;
    setLoadingShot(path);
    invoke<string>('read_screenshot', { path })
      .then(b64 => setScreenshots(prev => ({ ...prev, [path]: b64 })))
      .catch(() => {/* file may have been deleted */})
      .finally(() => setLoadingShot(null));
  }, [selected?.screenshotPath]);

  const handleLoad = async (id: string) => {
    await invoke('load_conversation', { id });
  };

  const fmt = (timestamp: number) =>
    new Date(timestamp * 1000).toLocaleString(undefined, {
      month: 'short', day: 'numeric',
      hour: '2-digit', minute: '2-digit',
    });

  return (
    <div className="hv-root">
      {/* Sidebar */}
      <div className="hv-sidebar">
        <div className="hv-sidebar-header">
          <span className="hv-title">History</span>
          <span className="hv-count">{records.length} / 20</span>
        </div>
        <div className="hv-list">
          {records.map(r => (
            <button
              key={r.id}
              className={`hv-item ${selectedId === r.id ? 'hv-item--active' : ''}`}
              onClick={() => setSelectedId(r.id)}
            >
              <div className="hv-item-time">{fmt(r.timestamp)}</div>
              <div className="hv-item-transcript">{r.transcript}</div>
              <div className="hv-item-preview">
                {r.response.length > 72 ? r.response.slice(0, 72) + '…' : r.response}
              </div>
            </button>
          ))}
          {records.length === 0 && (
            <div className="hv-empty">No conversations yet.<br />Use Ctrl+Shift+Space to start.</div>
          )}
        </div>
      </div>

      {/* Detail pane */}
      <div className="hv-detail">
        {selected ? (
          <>
            <div className="hv-detail-header">
              <span className="hv-detail-time">{fmt(selected.timestamp)}</span>
              <button className="hv-load-btn" onClick={() => handleLoad(selected.id)}>
                Load in Widget ↗
              </button>
            </div>

            {selected.screenshotPath && (
              <div className="hv-screenshot-wrap">
                {screenshots[selected.screenshotPath]
                  ? <img
                      className="hv-screenshot"
                      src={`data:image/png;base64,${screenshots[selected.screenshotPath]}`}
                      alt="Screenshot"
                    />
                  : <div className="hv-screenshot-placeholder">Loading screenshot…</div>
                }
              </div>
            )}

            <div className="hv-section">
              <div className="hv-label">You said</div>
              <div className="hv-transcript">{selected.transcript}</div>
            </div>

            {selected.toolCalls.length > 0 && (
              <div className="hv-section">
                <div className="hv-label">Tool calls</div>
                {selected.toolCalls.map((tc, i) => (
                  <div key={i} className={`hv-tool hv-tool--${tc.status}`}>
                    <div className="hv-tool-name">{tc.tool}</div>
                    <pre className="hv-tool-input">{tc.input}</pre>
                    {tc.output && <pre className="hv-tool-output">{tc.output}</pre>}
                  </div>
                ))}
              </div>
            )}

            <div className="hv-section">
              <div className="hv-label">Response</div>
              <div className="hv-response">{selected.response}</div>
            </div>
          </>
        ) : (
          <div className="hv-empty-detail">← Select a conversation to view details</div>
        )}
      </div>
    </div>
  );
}

export default HistoryView;
