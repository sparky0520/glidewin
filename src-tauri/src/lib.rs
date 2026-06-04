use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};

// Exclude the widget window from screen captures so xcap never sees it.
// WDA_EXCLUDEFROMCAPTURE (0x11) requires Windows 10 v2004+.
#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    fn SetWindowDisplayAffinity(hwnd: *mut std::ffi::c_void, affinity: u32) -> i32;
}

fn do_capture_screen() -> Result<String, String> {
    use xcap::Monitor;
    use std::time::{SystemTime, UNIX_EPOCH};

    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let monitor = monitors.first().ok_or("No monitors found")?;
    let image = monitor.capture_image().map_err(|e| e.to_string())?;

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let path = std::env::temp_dir().join(format!("glidewin_capture_{}.png", timestamp));
    image.save(&path).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn capture_screen() -> Result<String, String> {
    do_capture_screen()
}

// --- Audio helpers ---

// Encode i16 samples as an in-memory WAV file.
fn encode_wav_bytes(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    if let Ok(mut writer) = hound::WavWriter::new(&mut buf, spec) {
        for &s in samples {
            let _ = writer.write_sample(s);
        }
        let _ = writer.finalize();
    }
    buf.into_inner()
}

// Transcribe WAV bytes via OpenAI Whisper API.
async fn transcribe_bytes(api_key: &str, wav_bytes: Vec<u8>) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name("chunk.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("API request failed: {}", e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Whisper API error ({}): {}", status, body));
    }
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;
    Ok(json["text"].as_str().unwrap_or("").trim().to_string())
}

// --- Microphone Recording ---

struct RecordingHandle {
    stop_signal: Arc<Mutex<bool>>,
    thread_handle: Option<std::thread::JoinHandle<Result<(), String>>>,
    // Real-time transcription
    sample_queue: Arc<Mutex<Vec<i16>>>,
    transcript_stop_tx: tokio::sync::watch::Sender<bool>,
    transcript_task: tauri::async_runtime::JoinHandle<()>,
    accumulated_transcript: Arc<Mutex<String>>,
    sample_rate: u32,
    channels: u16,
    level_stop_tx: tokio::sync::watch::Sender<bool>,
    level_task: tauri::async_runtime::JoinHandle<()>,
}

struct RecorderState(Mutex<Option<RecordingHandle>>);

#[tauri::command]
fn start_recording(app: tauri::AppHandle, state: tauri::State<'_, RecorderState>) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Already recording".into());
    }

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let path = std::env::temp_dir().join(format!("glidewin_recording_{}.wav", timestamp));

    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("No microphone found")?;
    let supported_config = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels();

    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let writer = hound::WavWriter::create(&path, spec).map_err(|e| e.to_string())?;
    let writer = Arc::new(Mutex::new(Some(writer)));
    let stop_signal = Arc::new(Mutex::new(false));
    let sample_queue: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let accumulated_transcript: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let current_rms = Arc::new(AtomicU32::new(0));
    let current_rms_thread = current_rms.clone();

    let writer_clone = writer.clone();
    let stop_clone = stop_signal.clone();
    let sample_queue_thread = sample_queue.clone();

    let thread_handle = std::thread::spawn(move || -> Result<(), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No microphone found")?;
        let supported_config = device.default_input_config().map_err(|e| e.to_string())?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let writer_for_cb = writer_clone.clone();
        let queue_for_cb = sample_queue_thread.clone();

        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if let Ok(mut w) = writer_for_cb.lock() {
                        if let Some(ref mut writer) = *w {
                            for &sample in data { let _ = writer.write_sample(sample); }
                        }
                    }
                    if let Ok(mut q) = queue_for_cb.lock() {
                        q.extend_from_slice(data);
                    }
                    if !data.is_empty() {
                        let sq: f64 = data.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
                        let rms = (sq / data.len() as f64).sqrt() as f32;
                        current_rms_thread.store(rms.to_bits(), Ordering::Relaxed);
                    }
                },
                |err| eprintln!("Audio stream error: {}", err),
                None,
            ).map_err(|e| e.to_string())?,
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if let Ok(mut w) = writer_for_cb.lock() {
                        if let Some(ref mut writer) = *w {
                            for &s in data { let _ = writer.write_sample((s * i16::MAX as f32) as i16); }
                        }
                    }
                    if let Ok(mut q) = queue_for_cb.lock() {
                        for &s in data { q.push((s * i16::MAX as f32) as i16); }
                    }
                    if !data.is_empty() {
                        let sq: f64 = data.iter().map(|&s| (s as f64).powi(2)).sum();
                        let rms = (sq / data.len() as f64).sqrt() as f32;
                        current_rms_thread.store(rms.to_bits(), Ordering::Relaxed);
                    }
                },
                |err| eprintln!("Audio stream error: {}", err),
                None,
            ).map_err(|e| e.to_string())?,
            _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
        };

        stream.play().map_err(|e| e.to_string())?;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if *stop_clone.lock().unwrap() { break; }
        }

        drop(stream);

        if let Ok(mut w) = writer_clone.lock() {
            if let Some(writer) = w.take() {
                writer.finalize().map_err(|e| e.to_string())?;
            }
        }

        Ok(())
    });

    // Background task: every 3s drain the sample queue and emit a transcript chunk.
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let queue_task = sample_queue.clone();
    let accum_task = accumulated_transcript.clone();
    let app_task = app.clone();
    let sr = sample_rate;
    let ch = channels;
    // Minimum samples before bothering to transcribe (~2 seconds of audio).
    let min_samples = sr as usize * ch as usize * 2;

    let transcript_task = tauri::async_runtime::spawn(async move {
        use tauri::Emitter;

        loop {
            // Wait 3 seconds or until stop is signaled.
            tokio::select! {
                biased;
                _ = stop_rx.changed() => break,
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(3)) => {}
            }

            let api_key = match std::env::var("OPENAI_API_KEY") {
                Ok(k) => k,
                Err(_) => continue,
            };

            let samples: Vec<i16> = {
                let mut q = queue_task.lock().unwrap();
                std::mem::take(&mut *q)
            };

            if samples.len() < min_samples { continue; }

            let wav_bytes = encode_wav_bytes(&samples, sr, ch);

            if let Ok(text) = transcribe_bytes(&api_key, wav_bytes).await {
                if !text.is_empty() {
                    let mut accum = accum_task.lock().unwrap();
                    if !accum.is_empty() { accum.push(' '); }
                    accum.push_str(&text);
                    let chunk = text;
                    drop(accum);
                    app_task.emit("transcript-chunk", chunk).ok();
                }
            }
        }
    });

    // Level monitoring: emit normalized RMS every 100ms so the frontend can detect silence.
    let (level_tx, mut level_rx) = tokio::sync::watch::channel(false);
    let level_task = tauri::async_runtime::spawn(async move {
        use tauri::Emitter;
        loop {
            tokio::select! {
                biased;
                _ = level_rx.changed() => break,
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
            }
            let rms = f32::from_bits(current_rms.load(Ordering::Relaxed));
            app.emit("audio-level", rms).ok();
        }
    });

    *guard = Some(RecordingHandle {
        stop_signal,
        thread_handle: Some(thread_handle),
        sample_queue,
        transcript_stop_tx: stop_tx,
        transcript_task,
        accumulated_transcript,
        sample_rate: sr,
        channels: ch,
        level_stop_tx: level_tx,
        level_task,
    });
    Ok(())
}

#[tauri::command]
async fn stop_recording(app: tauri::AppHandle, state: tauri::State<'_, RecorderState>) -> Result<String, String> {
    use tauri::Emitter;

    let handle = {
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.take().ok_or("Not currently recording")?
    };

    // Stop the recording thread.
    *handle.stop_signal.lock().unwrap() = true;
    if let Some(thread) = handle.thread_handle {
        thread.join().map_err(|_| "Recording thread panicked".to_string())??;
    }

    // Stop background tasks.
    handle.transcript_stop_tx.send(true).ok();
    handle.level_stop_tx.send(true).ok();
    let _ = handle.transcript_task.await;
    let _ = handle.level_task.await;

    // Transcribe any samples that accumulated since the last periodic chunk.
    let remaining: Vec<i16> = std::mem::take(&mut *handle.sample_queue.lock().unwrap());
    let min_samples = handle.sample_rate as usize * handle.channels as usize / 2; // ~0.5s
    if remaining.len() >= min_samples {
        if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            let wav_bytes = encode_wav_bytes(&remaining, handle.sample_rate, handle.channels);
            if let Ok(text) = transcribe_bytes(&api_key, wav_bytes).await {
                if !text.is_empty() {
                    let mut accum = handle.accumulated_transcript.lock().unwrap();
                    if !accum.is_empty() { accum.push(' '); }
                    accum.push_str(&text);
                    let chunk = text;
                    drop(accum);
                    app.emit("transcript-chunk", chunk).ok();
                }
            }
        }
    }

    let final_transcript = handle.accumulated_transcript.lock().unwrap().clone();
    Ok(final_transcript)
}

// --- Speech Transcription (kept for potential direct use) ---

#[tauri::command]
async fn transcribe_audio(file_path: String) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;
    let file_bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read audio file: {}", e))?;
    let file_name = std::path::Path::new(&file_path)
        .file_name().unwrap_or_default().to_string_lossy().into_owned();
    let wav_bytes = file_bytes;
    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name(file_name).mime_str("audio/wav").map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().text("model", "whisper-1").part("file", part);
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key).multipart(form).send().await
        .map_err(|e| format!("API request failed: {}", e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Whisper API error ({}): {}", status, body));
    }
    let json: serde_json::Value = response.json().await
        .map_err(|e| format!("Failed to parse response: {}", e))?;
    Ok(json["text"].as_str().ok_or("No 'text' field in API response")?.to_string())
}

// --- GPT Integration (Streaming, visual mode) ---

#[tauri::command]
async fn ask_gpt_stream(
    app: tauri::AppHandle,
    screenshot_path: String,
    transcript: String,
) -> Result<(), String> {
    use base64::{Engine as _, engine::general_purpose};
    use futures_util::StreamExt;
    use tauri::Emitter;

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let image_bytes = std::fs::read(&screenshot_path)
        .map_err(|e| format!("Failed to read screenshot: {}", e))?;

    let base64_image = general_purpose::STANDARD.encode(&image_bytes);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64_image)}},
            {"type": "text", "text": transcript}
        ]}],
        "max_tokens": 1024,
        "stream": true
    });

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(&api_key).json(&body).send().await
        .map_err(|e| format!("API request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("GPT API error ({}): {}", status, body));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("Stream error: {}", e))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim_end_matches('\r').to_string();
            buffer = buffer[pos + 1..].to_string();

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" { return Ok(()); }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                        if !content.is_empty() { app.emit("gpt-token", content).ok(); }
                    }
                }
            }
        }
    }

    Ok(())
}

// --- Text-to-Speech ---

// Split text into sentences on '. ', '! ', '? ', or end-of-string.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            match chars.peek() {
                None | Some(' ') | Some('\n') => {
                    let s = current.trim().to_string();
                    if !s.is_empty() { sentences.push(s); }
                    current = String::new();
                }
                _ => {}
            }
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() { sentences.push(tail); }
    sentences
}

// Fetch TTS audio bytes for a single sentence from OpenAI.
async fn fetch_tts_bytes(api_key: &str, text: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": "tts-1",
            "input": text,
            "voice": "alloy",
            "response_format": "mp3"
        }))
        .send().await
        .map_err(|e| format!("TTS request failed: {}", e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("TTS API error ({}): {}", status, body));
    }
    response.bytes().await
        .map_err(|e| format!("Failed to read TTS audio: {}", e))
        .map(|b| b.to_vec())
}

#[tauri::command]
async fn speak_text(text: String) -> Result<(), String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let sentences: Vec<String> = split_sentences(&text)
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();

    if sentences.is_empty() { return Ok(()); }

    // Async→blocking bridge: TTS fetcher sends MP3 bytes; playback thread drains them in order.
    // Buffer up to 2 sentences so the fetcher stays ahead without blocking the async task.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);

    // One persistent audio device for the whole response — no clicks between sentences.
    let playback = std::thread::spawn(move || -> Result<(), String> {
        use rodio::{Decoder, OutputStream, Sink};
        use std::io::Cursor;
        let (_stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| format!("Audio output error: {}", e))?;
        while let Some(audio) = rx.blocking_recv() {
            let sink = Sink::try_new(&stream_handle)
                .map_err(|e| format!("Sink error: {}", e))?;
            let source = Decoder::new(Cursor::new(audio))
                .map_err(|e| format!("Decode error: {}", e))?;
            sink.append(source);
            sink.sleep_until_end();
        }
        Ok(())
    });

    // Fetch each sentence's TTS audio and hand it off to the playback thread.
    // Playback starts as soon as the first sentence is ready.
    for sentence in &sentences {
        let audio = fetch_tts_bytes(&api_key, sentence.trim()).await?;
        tx.send(audio).await.map_err(|_| "Playback thread closed early".to_string())?;
    }
    drop(tx); // signal playback thread to exit after draining the queue

    tauri::async_runtime::spawn_blocking(move || {
        playback.join().map_err(|_| "Playback thread panicked".to_string())?
    }).await.map_err(|e| format!("Playback join error: {}", e))?
}

// --- Skills System ---

fn skills_dir() -> PathBuf {
    let mut p = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("GlideWin");
    p.push("skills");
    std::fs::create_dir_all(&p).ok();
    p
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SkillDef {
    name: String,
    description: String,
    parameters: Vec<String>,
    powershell_code: String,
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut tag_buf = String::new();

    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                let lower = tag_buf.to_ascii_lowercase();
                let lower = lower.trim();
                if lower.starts_with("script") || lower.starts_with("style") {
                    in_script = true;
                } else if lower.starts_with("/script") || lower.starts_with("/style") {
                    in_script = false;
                }
                tag_buf.clear();
                in_tag = false;
                if !in_script {
                    out.push(' ');
                }
            } else {
                tag_buf.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if !in_script {
            out.push(ch);
        }
    }

    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- Agentic Loop (T0009) ---

use rig_derive::rig_tool;
use rig_core::tool::ToolError;

// Carries the AppHandle into tool functions via tokio task-local storage.
// Tools run in the same task as agent_chat, so try_with succeeds.
tokio::task_local! {
    static TOOL_APP_HANDLE: tauri::AppHandle;
}

fn emit_tool_event(tool: &str, input: &str, status: &str, output: Option<&str>) {
    use tauri::Emitter;
    TOOL_APP_HANDLE.try_with(|app| {
        let mut payload = serde_json::json!({ "tool": tool, "input": input, "status": status });
        if let Some(out) = output {
            payload["output"] = serde_json::Value::String(out.to_string());
        }
        app.emit("tool-call", payload).ok();
    }).ok();
}

/// Execute a PowerShell command on the Windows PC and return its output.
/// Use this to open apps, list files, get system info, run scripts, or automate anything on the PC.
#[rig_tool]
async fn run_powershell(
    /// The PowerShell command to run (e.g. "Get-Process", "notepad.exe", "dir C:\\Users")
    command: String,
) -> Result<String, ToolError> {
    emit_tool_event("run_powershell", &command, "start", None);

    let output = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            emit_tool_event("run_powershell", &command, "error", Some(&e.to_string()));
            return Err(ToolError::ToolCallError(e.to_string().into()));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let err = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            // Some tools write errors to stdout and exit non-zero
            format!("Exit code {}: {}", output.status.code().unwrap_or(-1), stdout.trim())
        } else {
            format!("Command failed with exit code {}", output.status.code().unwrap_or(-1))
        };
        emit_tool_event("run_powershell", &command, "error", Some(&err));
        return Err(ToolError::ToolCallError(err.into()));
    }

    // Include stderr warnings alongside stdout so the agent can see them
    let result = match (stdout.trim(), stderr.trim()) {
        ("", "") => "Done (no output).".to_string(),
        ("", err) => format!("(stderr) {}", err),
        (out, "") => out.to_string(),
        (out, err) => format!("{}\n(stderr) {}", out, err),
    };

    emit_tool_event("run_powershell", &command, "done", Some(&result));
    Ok(result)
}

/// Open an application or URL on Windows using the shell `start` command.
/// Always prefer this over run_powershell for launching apps or websites.
#[rig_tool]
async fn open_app(
    /// Application to open. Use the shell name exactly as you would type it at a command prompt:
    /// "chrome", "msedge", "firefox", "notepad", "explorer", "spotify", "code", etc.
    /// For a URL with no specific browser, pass "https://..." here and leave url empty.
    app: String,
    /// Optional URL to open with the app, e.g. "https://youtube.com".
    /// Leave empty when just launching an app without a URL.
    url: String,
) -> Result<String, ToolError> {
    let label = if url.is_empty() { app.clone() } else { format!("{} {}", app, url) };
    emit_tool_event("open_app", &label, "start", None);

    // Build `cmd /c start "" <app> [url]` with each token as a separate argument
    // so the shell never misreads a URL as a window title.
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/c", "start", "", &app]);
    if !url.is_empty() {
        cmd.arg(&url);
    }

    match cmd.output().await {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() && !stderr.trim().is_empty() {
                let err = stderr.trim().to_string();
                emit_tool_event("open_app", &label, "error", Some(&err));
                return Err(ToolError::ToolCallError(err.into()));
            }
            let msg = format!("Opened: {}", label);
            emit_tool_event("open_app", &label, "done", Some(&msg));
            Ok(msg)
        }
        Err(e) => {
            emit_tool_event("open_app", &label, "error", Some(&e.to_string()));
            Err(ToolError::ToolCallError(e.to_string().into()))
        }
    }
}

/// List all saved skills with their names and descriptions.
#[rig_tool]
async fn list_skills() -> Result<String, ToolError> {
    emit_tool_event("list_skills", "", "start", None);
    let dir = skills_dir();
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(skill) = serde_json::from_str::<SkillDef>(&content) {
                        let params = if skill.parameters.is_empty() {
                            String::new()
                        } else {
                            format!(" (params: {})", skill.parameters.join(", "))
                        };
                        skills.push(format!("{}{}: {}", skill.name, params, skill.description));
                    }
                }
            }
        }
    }
    let result = if skills.is_empty() {
        "No skills saved yet.".to_string()
    } else {
        skills.join("\n")
    };
    emit_tool_event("list_skills", "", "done", Some(&result));
    Ok(result)
}

/// Save a reusable PowerShell procedure as a named skill for future use.
#[rig_tool]
async fn create_skill(
    /// Short identifier for the skill, no spaces (e.g. "get_battery")
    name: String,
    /// One-sentence description of what the skill does
    description: String,
    /// JSON array of parameter names the skill accepts, e.g. ["query"] or []
    parameters: String,
    /// PowerShell script for the skill. Reference parameters as $paramname variables.
    powershell_code: String,
) -> Result<String, ToolError> {
    emit_tool_event("create_skill", &name, "start", None);

    let params: Vec<String> = serde_json::from_str(&parameters).unwrap_or_default();
    let skill = SkillDef { name: name.clone(), description, parameters: params, powershell_code };
    let path = skills_dir().join(format!("{}.json", name));
    let json = serde_json::to_string_pretty(&skill)
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;
    std::fs::write(&path, json)
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let msg = format!("Skill '{}' saved.", name);
    emit_tool_event("create_skill", &name, "done", Some(&msg));
    Ok(msg)
}

/// Run a saved skill by name, passing parameters as a JSON object.
#[rig_tool]
async fn use_skill(
    /// The skill name to run
    name: String,
    /// Parameters as a JSON object, e.g. {"query": "hello"} or {}
    params: String,
) -> Result<String, ToolError> {
    emit_tool_event("use_skill", &name, "start", None);

    let path = skills_dir().join(format!("{}.json", name));
    let content = std::fs::read_to_string(&path)
        .map_err(|_| ToolError::ToolCallError(format!("Skill '{}' not found.", name).into()))?;
    let skill: SkillDef = serde_json::from_str(&content)
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let param_map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&params).unwrap_or_default();

    let mut preamble = String::new();
    for (k, v) in &param_map {
        let val = match v {
            serde_json::Value::String(s) => {
                let escaped = s.replace('`', "``").replace('"', "`\"").replace('$', "`$");
                format!("\"{}\"", escaped)
            }
            other => other.to_string(),
        };
        preamble.push_str(&format!("${} = {};\n", k, val));
    }

    let full_command = format!("{}{}", preamble, skill.powershell_code);

    let output = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &full_command])
        .output()
        .await
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let err = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        emit_tool_event("use_skill", &name, "error", Some(&err));
        return Err(ToolError::ToolCallError(err.into()));
    }

    let result = match (stdout.trim(), stderr.trim()) {
        ("", "") => "Done (no output).".to_string(),
        ("", err) => format!("(stderr) {}", err),
        (out, "") => out.to_string(),
        (out, err) => format!("{}\n(stderr) {}", out, err),
    };

    emit_tool_event("use_skill", &name, "done", Some(&result));
    Ok(result)
}

/// Fetch the plain-text content of a web page.
#[rig_tool]
async fn web_fetch(
    /// The URL to fetch
    url: String,
) -> Result<String, ToolError> {
    emit_tool_event("web_fetch", &url, "start", None);

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .build()
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let html = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?
        .text()
        .await
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let text = strip_html_tags(&html);
    let truncated = if text.len() > 3000 {
        format!("{}... [truncated — {} chars remaining. Call web_fetch again on a more specific URL if you need more.]", &text[..3000], text.len() - 3000)
    } else {
        text
    };

    emit_tool_event("web_fetch", &url, "done", Some(&format!("{} chars", truncated.len())));
    Ok(truncated)
}

/// Search the web and return top results with titles, URLs, and descriptions.
#[rig_tool]
async fn web_search(
    /// The search query
    query: String,
) -> Result<String, ToolError> {
    emit_tool_event("web_search", &query, "start", None);

    let api_key = std::env::var("BRAVE_API_KEY").map_err(|_| {
        ToolError::ToolCallError(
            "BRAVE_API_KEY not set. Add it to .env to enable web search.".into(),
        )
    })?;

    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query.as_str()), ("count", "5")])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ToolError::ToolCallError(e.to_string().into()))?;

    let results = json["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, r)| {
                    let title = r["title"].as_str().unwrap_or("(no title)");
                    let url = r["url"].as_str().unwrap_or("");
                    let desc = r["description"].as_str().unwrap_or("");
                    format!("{}. {} ({})\n   {}", i + 1, title, url, desc)
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No results found.".to_string());

    emit_tool_event("web_search", &query, "done", Some(&results));
    Ok(results)
}

struct ConversationState(tokio::sync::Mutex<Vec<rig_core::completion::Message>>);

#[tauri::command]
async fn agent_chat(
    app: tauri::AppHandle,
    state: tauri::State<'_, ConversationState>,
    message: String,
    screenshot_path: Option<String>,
) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose};
    use rig_core::{
        client::{CompletionClient, ProviderClient},
        completion::{Chat, Message},
        completion::message::{DocumentSourceKind, Image, ImageMediaType, Text, UserContent},
        providers::openai,
        OneOrMany,
    };
    use tauri::Emitter;

    let client = openai::Client::from_env().map_err(|e| e.to_string())?;

    let agent = client
        .agent(openai::GPT_4O)
        .preamble(
            "You are GlideWin, an AI assistant running on the user's Windows PC. \
             \
             SYSTEM TOOLS: run_powershell for one-off commands; open_app for launching apps or \
             websites. To open an app: app=\"chrome\", url=\"\". To open a URL in a specific \
             browser: app=\"chrome\", url=\"https://...\". To open a URL in the default browser: \
             app=\"https://...\" and url=\"\". Always prefer open_app over run_powershell for \
             launching applications. \
             \
             WEB TOOLS: Always use web_search first — its title and snippet results are usually \
             sufficient. Only call web_fetch when you genuinely need to read the full content of \
             a specific page (e.g. documentation, a tutorial, or code). web_fetch is capped at \
             3000 chars and will tell you if content was truncated. \
             \
             SKILL SYSTEM: Skills are saved, reusable PowerShell procedures. \
             Use list_skills to see what's available. \
             Use use_skill to run a skill by name with a JSON params object. \
             Use create_skill to save a new reusable procedure — do this whenever you solve a \
             task that is likely to be repeated. Skills use $paramname PowerShell variables. \
             \
             Always tell the user what you are about to do before calling a tool. \
             Keep responses concise. Never delete files or make destructive changes \
             without explicit user confirmation.",
        )
        .max_tokens(2048)
        .default_max_turns(10)
        .tool(RunPowershell)
        .tool(OpenApp)
        .tool(ListSkills)
        .tool(CreateSkill)
        .tool(UseSkill)
        .tool(WebFetch)
        .tool(WebSearch)
        .build();

    let user_content: OneOrMany<UserContent> = match screenshot_path {
        Some(path) => {
            let img_bytes = std::fs::read(&path)
                .map_err(|e| format!("Failed to read screenshot: {}", e))?;
            let b64 = general_purpose::STANDARD.encode(&img_bytes);
            OneOrMany::many(vec![
                UserContent::Image(Image {
                    data: DocumentSourceKind::Base64(b64),
                    media_type: Some(ImageMediaType::PNG),
                    detail: None,
                    additional_params: None,
                }),
                UserContent::Text(Text { text: message.clone(), additional_params: None }),
            ]).map_err(|e| e.to_string())?
        }
        None => OneOrMany::one(UserContent::Text(Text { text: message.clone(), additional_params: None })),
    };

    let prompt_msg = Message::User { content: user_content };

    // Clone history so we don't hold the mutex across the API call
    let mut history = state.0.lock().await.clone();

    app.emit("agent-thinking", true).ok();

    // Run the agent inside the task-local scope so tools can emit events
    let (response_result, updated_history) = TOOL_APP_HANDLE
        .scope(app.clone(), async move {
            let resp = agent.chat(prompt_msg, &mut history).await;
            (resp, history)
        })
        .await;

    app.emit("agent-thinking", false).ok();

    let response = response_result.map_err(|e| e.to_string())?;

    // Persist the updated history
    *state.0.lock().await = updated_history;

    Ok(response)
}

#[tauri::command]
async fn clear_conversation(state: tauri::State<'_, ConversationState>) -> Result<(), String> {
    state.0.lock().await.clear();
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    dotenvy::dotenv().ok();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .manage(RecorderState(Mutex::new(None)))
        .manage(ConversationState(tokio::sync::Mutex::new(Vec::new())))
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

            let window = app.get_webview_window("main").unwrap();

            // Pin widget to top-center of primary monitor
            if let Ok(Some(monitor)) = window.primary_monitor() {
                let logical_w = monitor.size().width as f64 / monitor.scale_factor();
                let x = (logical_w / 2.0 - 240.0).max(0.0);
                window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y: 20.0 }))?;
            }

            // Exclude widget from screen captures so screenshots never contain it
            #[cfg(target_os = "windows")]
            if let Ok(hwnd) = window.hwnd() {
                const WDA_EXCLUDEFROMCAPTURE: u32 = 0x00000011;
                unsafe { SetWindowDisplayAffinity(hwnd.0, WDA_EXCLUDEFROMCAPTURE); }
            }

            // Register Ctrl+Shift+Space once at the Rust level so React lifecycle
            // (StrictMode double-mount, hot-reload) can never cause "already registered" errors.
            let handle = app.handle().clone();
            app.global_shortcut().on_shortcut("CommandOrControl+Shift+Space", move |_app, _shortcut, event| {
                if event.state() != ShortcutState::Pressed { return; }
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tauri::Emitter;
                    if let Some(win) = handle.get_webview_window("main") {
                        if win.is_visible().unwrap_or(false) {
                            let _ = win.hide();
                        } else {
                            // Widget is excluded from capture via WDA_EXCLUDEFROMCAPTURE —
                            // capture immediately, no hide/sleep needed.
                            let path = tokio::task::spawn_blocking(do_capture_screen)
                                .await
                                .ok()
                                .and_then(|r| r.ok())
                                .unwrap_or_default();
                            let _ = win.show();
                            let _ = win.set_focus();
                            let _ = win.emit("activate", path);
                        }
                    }
                });
            })?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            capture_screen,
            start_recording,
            stop_recording,
            transcribe_audio,
            ask_gpt_stream,
            speak_text,
            agent_chat,
            clear_conversation,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
