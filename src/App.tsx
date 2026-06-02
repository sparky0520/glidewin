import { useEffect, useState } from 'react';
import { register, unregisterAll } from '@tauri-apps/plugin-global-shortcut';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './App.css';

function App() {
  const [error, setError] = useState<string | null>(null);

  const dismissWindow = async () => {
    try {
      const appWindow = getCurrentWindow();
      await appWindow.hide();
    } catch (e: any) {
      setError(e.toString());
    }
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
      {error && <div style={{ color: 'red', marginTop: '1rem' }}><strong>Error:</strong> {error}</div>}
      <button onClick={dismissWindow}>Dismiss (Esc)</button>
    </div>
  );
}

export default App;
