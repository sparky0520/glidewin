use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// DWMWA_CAPTION_COLOR (35) sets the title-bar background; requires Windows 11 Build 22000+.
#[cfg(target_os = "windows")]
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut std::ffi::c_void,
        dw_attribute: u32,
        pv_attribute: *const std::ffi::c_void,
        cb_attribute: u32,
    ) -> i32;
}

fn do_capture_screen() -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    use xcap::Monitor;

    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let monitor = monitors.first().ok_or("No monitors found")?;
    let image = monitor.capture_image().map_err(|e| e.to_string())?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = std::env::temp_dir().join(format!("sharp_capture_{}.png", timestamp));
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
fn start_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecorderState>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Already recording".into());
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = std::env::temp_dir().join(format!("sharp_recording_{}.wav", timestamp));

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
            cpal::SampleFormat::I16 => device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut w) = writer_for_cb.lock() {
                            if let Some(ref mut writer) = *w {
                                for &sample in data {
                                    let _ = writer.write_sample(sample);
                                }
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
                )
                .map_err(|e| e.to_string())?,
            cpal::SampleFormat::F32 => device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut w) = writer_for_cb.lock() {
                            if let Some(ref mut writer) = *w {
                                for &s in data {
                                    let _ = writer.write_sample((s * i16::MAX as f32) as i16);
                                }
                            }
                        }
                        if let Ok(mut q) = queue_for_cb.lock() {
                            for &s in data {
                                q.push((s * i16::MAX as f32) as i16);
                            }
                        }
                        if !data.is_empty() {
                            let sq: f64 = data.iter().map(|&s| (s as f64).powi(2)).sum();
                            let rms = (sq / data.len() as f64).sqrt() as f32;
                            current_rms_thread.store(rms.to_bits(), Ordering::Relaxed);
                        }
                    },
                    |err| eprintln!("Audio stream error: {}", err),
                    None,
                )
                .map_err(|e| e.to_string())?,
            _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
        };

        stream.play().map_err(|e| e.to_string())?;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if *stop_clone.lock().unwrap() {
                break;
            }
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

            if samples.len() < min_samples {
                continue;
            }

            let wav_bytes = encode_wav_bytes(&samples, sr, ch);

            if let Ok(text) = transcribe_bytes(&api_key, wav_bytes).await {
                if !text.is_empty() {
                    let mut accum = accum_task.lock().unwrap();
                    if !accum.is_empty() {
                        accum.push(' ');
                    }
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
async fn stop_recording(
    app: tauri::AppHandle,
    state: tauri::State<'_, RecorderState>,
) -> Result<String, String> {
    use tauri::Emitter;

    let handle = {
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.take().ok_or("Not currently recording")?
    };

    // Stop the recording thread.
    *handle.stop_signal.lock().unwrap() = true;
    if let Some(thread) = handle.thread_handle {
        thread
            .join()
            .map_err(|_| "Recording thread panicked".to_string())??;
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
                    if !accum.is_empty() {
                        accum.push(' ');
                    }
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
    let file_bytes =
        std::fs::read(&file_path).map_err(|e| format!("Failed to read audio file: {}", e))?;
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let wav_bytes = file_bytes;
    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name(file_name)
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);
    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(&api_key)
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
    Ok(json["text"]
        .as_str()
        .ok_or("No 'text' field in API response")?
        .to_string())
}

// --- GPT Integration (Streaming, visual mode) ---

#[tauri::command]
async fn ask_gpt_stream(
    app: tauri::AppHandle,
    screenshot_path: String,
    transcript: String,
) -> Result<(), String> {
    use base64::{engine::general_purpose, Engine as _};
    use futures_util::StreamExt;
    use tauri::Emitter;

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let image_bytes =
        std::fs::read(&screenshot_path).map_err(|e| format!("Failed to read screenshot: {}", e))?;

    let base64_image = general_purpose::STANDARD.encode(&image_bytes);

    let body = serde_json::json!({
        "model": "gpt-5-nano",
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
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
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
                if data == "[DONE]" {
                    return Ok(());
                }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                        if !content.is_empty() {
                            app.emit("gpt-token", content).ok();
                        }
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
                    if !s.is_empty() {
                        sentences.push(s);
                    }
                    current = String::new();
                }
                _ => {}
            }
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        sentences.push(tail);
    }
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
        .send()
        .await
        .map_err(|e| format!("TTS request failed: {}", e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("TTS API error ({}): {}", status, body));
    }
    response
        .bytes()
        .await
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

    if sentences.is_empty() {
        return Ok(());
    }

    // Async→blocking bridge: TTS fetcher sends MP3 bytes; playback thread drains them in order.
    // Buffer up to 2 sentences so the fetcher stays ahead without blocking the async task.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);

    // One persistent audio device for the whole response — no clicks between sentences.
    let playback = std::thread::spawn(move || -> Result<(), String> {
        use rodio::{Decoder, OutputStream, Sink};
        use std::io::Cursor;
        let (_stream, stream_handle) =
            OutputStream::try_default().map_err(|e| format!("Audio output error: {}", e))?;
        while let Some(audio) = rx.blocking_recv() {
            let sink = Sink::try_new(&stream_handle).map_err(|e| format!("Sink error: {}", e))?;
            let source =
                Decoder::new(Cursor::new(audio)).map_err(|e| format!("Decode error: {}", e))?;
            sink.append(source);
            sink.sleep_until_end();
        }
        Ok(())
    });

    // Fetch each sentence's TTS audio and hand it off to the playback thread.
    // Playback starts as soon as the first sentence is ready.
    for sentence in &sentences {
        let audio = fetch_tts_bytes(&api_key, sentence.trim()).await?;
        tx.send(audio)
            .await
            .map_err(|_| "Playback thread closed early".to_string())?;
    }
    drop(tx); // signal playback thread to exit after draining the queue

    tauri::async_runtime::spawn_blocking(move || {
        playback
            .join()
            .map_err(|_| "Playback thread panicked".to_string())?
    })
    .await
    .map_err(|e| format!("Playback join error: {}", e))?
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

// --- Agentic Loop ---

tokio::task_local! {
    static TOOL_APP_HANDLE: tauri::AppHandle;
}

fn emit_tool_event(tool: &str, input: &str, status: &str, output: Option<&str>) {
    use tauri::Emitter;
    TOOL_APP_HANDLE
        .try_with(|app| {
            let mut payload = serde_json::json!({ "tool": tool, "input": input, "status": status });
            if let Some(out) = output {
                payload["output"] = serde_json::Value::String(out.to_string());
            }
            app.emit("tool-call", payload).ok();
        })
        .ok();
}

// ── Tool implementations ──────────────────────────────────────────────────────

async fn tool_run_powershell(command: String) -> Result<String, String> {
    emit_tool_event("run_powershell", &command, "start", None);

    let output = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            emit_tool_event("run_powershell", &command, "error", Some(&e.to_string()));
            return Err(e.to_string());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let err = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            format!(
                "Exit code {}: {}",
                output.status.code().unwrap_or(-1),
                stdout.trim()
            )
        } else {
            format!(
                "Command failed with exit code {}",
                output.status.code().unwrap_or(-1)
            )
        };
        emit_tool_event("run_powershell", &command, "error", Some(&err));
        return Err(err);
    }

    let result = match (stdout.trim(), stderr.trim()) {
        ("", "") => "Done (no output).".to_string(),
        ("", err) => format!("(stderr) {}", err),
        (out, "") => out.to_string(),
        (out, err) => format!("{}\n(stderr) {}", out, err),
    };

    emit_tool_event("run_powershell", &command, "done", Some(&result));
    Ok(result)
}

async fn tool_open_app(app: String, url: String) -> Result<String, String> {
    let label = if url.is_empty() {
        app.clone()
    } else {
        format!("{} {}", app, url)
    };
    emit_tool_event("open_app", &label, "start", None);

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
                return Err(err);
            }
            let msg = format!("Opened: {}", label);
            emit_tool_event("open_app", &label, "done", Some(&msg));
            Ok(msg)
        }
        Err(e) => {
            emit_tool_event("open_app", &label, "error", Some(&e.to_string()));
            Err(e.to_string())
        }
    }
}

async fn tool_list_skills() -> Result<String, String> {
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

async fn tool_create_skill(
    name: String,
    description: String,
    parameters: String,
    powershell_code: String,
) -> Result<String, String> {
    emit_tool_event("create_skill", &name, "start", None);
    let params: Vec<String> = serde_json::from_str(&parameters).unwrap_or_default();
    let skill = SkillDef {
        name: name.clone(),
        description,
        parameters: params,
        powershell_code,
    };
    let path = skills_dir().join(format!("{}.json", name));
    let json = serde_json::to_string_pretty(&skill).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    let msg = format!("Skill '{}' saved.", name);
    emit_tool_event("create_skill", &name, "done", Some(&msg));
    Ok(msg)
}

async fn tool_use_skill(name: String, params: String) -> Result<String, String> {
    emit_tool_event("use_skill", &name, "start", None);
    let path = skills_dir().join(format!("{}.json", name));
    let content =
        std::fs::read_to_string(&path).map_err(|_| format!("Skill '{}' not found.", name))?;
    let skill: SkillDef = serde_json::from_str(&content).map_err(|e| e.to_string())?;

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
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let err = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        emit_tool_event("use_skill", &name, "error", Some(&err));
        return Err(err);
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

async fn tool_web_fetch(url: String) -> Result<String, String> {
    emit_tool_event("web_fetch", &url, "start", None);
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .build()
        .map_err(|e| e.to_string())?;
    let html = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let text = strip_html_tags(&html);
    let truncated = if text.len() > 3000 {
        format!(
            "{}... [truncated — {} chars remaining]",
            &text[..3000],
            text.len() - 3000
        )
    } else {
        text
    };
    emit_tool_event(
        "web_fetch",
        &url,
        "done",
        Some(&format!("{} chars", truncated.len())),
    );
    Ok(truncated)
}

async fn tool_web_search(query: String) -> Result<String, String> {
    emit_tool_event("web_search", &query, "start", None);
    let api_key = std::env::var("BRAVE_API_KEY")
        .map_err(|_| "BRAVE_API_KEY not set. Add it to .env to enable web search.".to_string())?;
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query.as_str()), ("count", "5")])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let results = json["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, r)| {
                    format!(
                        "{}. {} ({})\n   {}",
                        i + 1,
                        r["title"].as_str().unwrap_or("(no title)"),
                        r["url"].as_str().unwrap_or(""),
                        r["description"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No results found.".to_string());
    emit_tool_event("web_search", &query, "done", Some(&results));
    Ok(results)
}

// ── GUI Control Tools ─────────────────────────────────────────────────────────

async fn tool_mouse_click(x: i32, y: i32, button: String) -> Result<String, String> {
    let label = format!("{} click at ({}, {})", button, x, y);
    emit_tool_event("mouse_click", &label, "start", None);
    let result = tokio::task::spawn_blocking(move || {
        use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};
        let mut e = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        e.move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| e.to_string())?;
        let btn = match button.to_lowercase().as_str() {
            "right" => Button::Right,
            "middle" => Button::Middle,
            _ => Button::Left,
        };
        e.button(btn, Direction::Click).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Clicked at ({}, {})", x, y))
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok(r) => emit_tool_event("mouse_click", &label, "done", Some(r)),
        Err(e) => emit_tool_event("mouse_click", &label, "error", Some(e)),
    }
    result
}

async fn tool_mouse_move(x: i32, y: i32) -> Result<String, String> {
    let label = format!("({}, {})", x, y);
    emit_tool_event("mouse_move", &label, "start", None);
    let result = tokio::task::spawn_blocking(move || {
        use enigo::{Coordinate, Enigo, Mouse, Settings};
        let mut e = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        e.move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Mouse moved to ({}, {})", x, y))
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok(r) => emit_tool_event("mouse_move", &label, "done", Some(r)),
        Err(e) => emit_tool_event("mouse_move", &label, "error", Some(e)),
    }
    result
}

async fn tool_type_text(text: String) -> Result<String, String> {
    let preview = if text.len() > 40 {
        format!("{}…", &text[..40])
    } else {
        text.clone()
    };
    emit_tool_event("type_text", &preview, "start", None);
    let text_clone = text.clone();
    let result = tokio::task::spawn_blocking(move || {
        use enigo::{Enigo, Keyboard, Settings};
        let mut e = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        e.text(&text_clone).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Typed {} chars", text_clone.len()))
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok(r) => emit_tool_event("type_text", &preview, "done", Some(r)),
        Err(e) => emit_tool_event("type_text", &preview, "error", Some(e)),
    }
    result
}

fn parse_enigo_key(s: &str) -> enigo::Key {
    use enigo::Key;
    match s.to_lowercase().as_str() {
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        c if c.len() == 1 => Key::Unicode(c.chars().next().unwrap()),
        _ => Key::Unicode(' '),
    }
}

async fn tool_key_combo(keys: String) -> Result<String, String> {
    emit_tool_event("key_combo", &keys, "start", None);
    let keys_clone = keys.clone();
    let result = tokio::task::spawn_blocking(move || {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        let mut e = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        let parts: Vec<String> = keys_clone
            .split('+')
            .map(|s| s.trim().to_lowercase())
            .collect();
        let (modifiers, main) = parts.split_at(parts.len().saturating_sub(1));

        for m in modifiers {
            let k = match m.as_str() {
                "ctrl" | "control" => Key::Control,
                "shift" => Key::Shift,
                "alt" => Key::Alt,
                "win" | "super" | "meta" | "cmd" => Key::Meta,
                _ => continue,
            };
            e.key(k, Direction::Press).map_err(|e| e.to_string())?;
        }
        if let Some(k) = main.first() {
            e.key(parse_enigo_key(k.as_str()), Direction::Click)
                .map_err(|e| e.to_string())?;
        }
        for m in modifiers.iter().rev() {
            let k = match m.as_str() {
                "ctrl" | "control" => Key::Control,
                "shift" => Key::Shift,
                "alt" => Key::Alt,
                "win" | "super" | "meta" | "cmd" => Key::Meta,
                _ => continue,
            };
            e.key(k, Direction::Release).map_err(|e| e.to_string())?;
        }
        Ok::<String, String>(format!("Pressed: {}", keys_clone))
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok(r) => emit_tool_event("key_combo", &keys, "done", Some(r)),
        Err(e) => emit_tool_event("key_combo", &keys, "error", Some(e)),
    }
    result
}

async fn tool_take_screenshot() -> Result<String, String> {
    emit_tool_event("take_screenshot", "", "start", None);
    let path = tokio::task::spawn_blocking(do_capture_screen)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e)?;
    emit_tool_event("take_screenshot", "", "done", Some("Screen captured."));
    Ok(path)
}

// ── Tool dispatch & schemas ───────────────────────────────────────────────────

enum ToolResult {
    Text(String),
    Screenshot(String),
}

async fn dispatch_tool(name: &str, args: &serde_json::Value) -> ToolResult {
    let s = |key: &str| args[key].as_str().unwrap_or("").to_string();
    let i = |key: &str| args[key].as_i64().unwrap_or(0) as i32;

    if name == "take_screenshot" {
        return match tool_take_screenshot().await {
            Ok(path) => ToolResult::Screenshot(path),
            Err(e) => ToolResult::Text(format!("Screenshot failed: {}", e)),
        };
    }

    if name == "think" {
        let thought = args["thought"].as_str().unwrap_or("");
        emit_tool_event("think", thought, "done", None);
        return ToolResult::Text("Thought noted.".to_string());
    }

    let result: Result<String, String> = match name {
        "run_powershell" => tool_run_powershell(s("command")).await,
        "open_app" => tool_open_app(s("app"), s("url")).await,
        "list_skills" => tool_list_skills().await,
        "create_skill" => {
            tool_create_skill(
                s("name"),
                s("description"),
                s("parameters"),
                s("powershell_code"),
            )
            .await
        }
        "use_skill" => tool_use_skill(s("name"), s("params")).await,
        "web_search" => tool_web_search(s("query")).await,
        "web_fetch" => tool_web_fetch(s("url")).await,
        "mouse_click" => tool_mouse_click(i("x"), i("y"), s("button")).await,
        "mouse_move" => tool_mouse_move(i("x"), i("y")).await,
        "type_text" => tool_type_text(s("text")).await,
        "key_combo" => tool_key_combo(s("keys")).await,
        _ => Err(format!("Unknown tool: {}", name)),
    };

    ToolResult::Text(result.unwrap_or_else(|e| format!("Error: {}", e)))
}

fn tool_schemas() -> serde_json::Value {
    serde_json::json!([
        {"type":"function","function":{"name":"run_powershell","description":"Execute a PowerShell command for system info, file ops, or automation. Use open_app for launching apps.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}},
        {"type":"function","function":{"name":"open_app","description":"Open an app or URL. App only: app='chrome', url=''. URL in browser: app='chrome', url='https://...'. Default browser: app='https://...', url=''.","parameters":{"type":"object","properties":{"app":{"type":"string"},"url":{"type":"string"}},"required":["app","url"]}}},
        {"type":"function","function":{"name":"list_skills","description":"List all saved reusable skills.","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{"name":"create_skill","description":"Save a reusable PowerShell procedure as a named skill. Use $paramname variables. Call this whenever you solve a task likely to be repeated.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"Short id, no spaces"},"description":{"type":"string"},"parameters":{"type":"string","description":"JSON array e.g. [\"query\"] or []"},"powershell_code":{"type":"string"}},"required":["name","description","parameters","powershell_code"]}}},
        {"type":"function","function":{"name":"use_skill","description":"Run a saved skill by name.","parameters":{"type":"object","properties":{"name":{"type":"string"},"params":{"type":"string","description":"JSON object e.g. {} or {\"query\":\"hello\"}"}},"required":["name","params"]}}},
        {"type":"function","function":{"name":"web_search","description":"Search the web. Use this first — snippets are usually enough. Only use web_fetch to read a full page.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}},
        {"type":"function","function":{"name":"web_fetch","description":"Fetch plain text from a URL, capped at 3000 chars. Only call when you need a full page (docs, code, articles).","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}}},
        {"type":"function","function":{"name":"mouse_click","description":"Click at absolute screen coordinates. Call take_screenshot first if you need to locate the target.","parameters":{"type":"object","properties":{"x":{"type":"integer"},"y":{"type":"integer"},"button":{"type":"string","enum":["left","right","middle"]}},"required":["x","y","button"]}}},
        {"type":"function","function":{"name":"mouse_move","description":"Move the mouse without clicking.","parameters":{"type":"object","properties":{"x":{"type":"integer"},"y":{"type":"integer"}},"required":["x","y"]}}},
        {"type":"function","function":{"name":"type_text","description":"Type text at the current cursor position. Focus the target field first.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}},
        {"type":"function","function":{"name":"key_combo","description":"Press a key or combination: 'ctrl+c', 'alt+f4', 'win+d', 'enter', 'escape', 'ctrl+shift+t'.","parameters":{"type":"object","properties":{"keys":{"type":"string"}},"required":["keys"]}}},
        {"type":"function","function":{"name":"take_screenshot","description":"Capture a fresh screenshot to see current screen state. Call after clicking or typing to verify the result.","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{"name":"think","description":"Use only when something unexpected happens and you need to reason through how to recover. Do not use for planning or narrating normal steps.","parameters":{"type":"object","properties":{"thought":{"type":"string","description":"Your recovery reasoning."}},"required":["thought"]}}},
        {"type":"function","function":{"name":"task_complete","description":"Call this when the task is fully finished. Pass a brief confirmation message to relay to the user. This ends the agent loop.","parameters":{"type":"object","properties":{"message":{"type":"string","description":"Short confirmation of what was done."}},"required":["message"]}}}
    ])
}

// ── OpenAI completion & agent loop ───────────────────────────────────────────

fn strip_images(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    messages.iter().map(|msg| {
        match msg["content"].as_array() {
            Some(parts) => {
                let text_only: Vec<serde_json::Value> = parts.iter()
                    .filter(|p| p["type"].as_str() != Some("image_url"))
                    .cloned()
                    .collect();
                let mut m = msg.clone();
                m["content"] = if text_only.is_empty() {
                    serde_json::json!("[screenshot]")
                } else {
                    serde_json::json!(text_only)
                };
                m
            }
            None => msg.clone(),
        }
    }).collect()
}

async fn call_openai(
    api_key: &str,
    system_prompt: &str,
    messages: &[serde_json::Value],
    tools: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut all_messages = vec![serde_json::json!({"role":"system","content":system_prompt})];
    all_messages.extend(strip_images(messages));

    let body = serde_json::json!({
        "model": "gpt-5-nano",
        "messages": all_messages,
        "tools": tools,
        "max_completion_tokens": 2048,
    });

    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OpenAI error ({}): {}", status, body));
    }

    resp.json().await.map_err(|e| e.to_string())
}

const SYSTEM_PROMPT: &str = "\
You are GlideWin, a voice-controlled AI assistant on the user's Windows PC.\n\
For GUI tasks, take a screenshot after each step to confirm the result before continuing. \
Only call think if something unexpected happens and you need to recover.\n\
APPS: Always prefer the installed desktop app over a website (e.g. open Spotify, not spotify.com). \
Use open_app to launch apps; only fall back to a URL if no app is installed.\n\
SYSTEM: Use run_powershell for system operations, file tasks, and scripting.\n\
WEB: Use web_search first — snippets are usually enough. Only use web_fetch when you need the full page content.\n\
SKILLS: Check list_skills before starting a task — a skill may already exist. \
Save a new skill with create_skill whenever you solve something likely to be repeated.\n\
GUI: Focus the target window or field before typing. After every click or keystroke, take a screenshot to confirm the result.\n\
Complete tasks fully from start to finish. Do not narrate your steps, announce what you are about to do, or give the user instructions — just act. \
When the task is fully done, call task_complete with a brief confirmation message. Never delete files or make destructive changes without explicit confirmation.\n\
EXAMPLE — User says \"Play Back in Black on Spotify\":\n\
1. open_app: open Spotify.\n\
2. take_screenshot: see the loaded app.\n\
3. mouse_click: click the search bar.\n\
4. type_text: type the song name.\n\
5. take_screenshot: see the search results.\n\
6. mouse_click: click play on the correct result.\n\
7. take_screenshot: confirm the song is playing.\n\
8. task_complete: \"Playing Back in Black on Spotify.\"";

async fn run_agent_loop(
    api_key: &str,
    history: &mut Vec<serde_json::Value>,
    tools: &serde_json::Value,
    max_turns: u32,
) -> Result<String, String> {
    use base64::{engine::general_purpose, Engine as _};

    for _ in 0..max_turns {
        let resp = call_openai(api_key, SYSTEM_PROMPT, history, tools).await?;

        let choice = resp["choices"][0].clone();
        let msg = choice["message"].clone();
        let finish = choice["finish_reason"].as_str().unwrap_or("stop");

        history.push(msg.clone());

        if finish == "stop" || finish == "end_turn" {
            return Ok(msg["content"].as_str().unwrap_or("").to_string());
        }

        if finish == "tool_calls" {
            let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
            let mut screenshot_paths: Vec<String> = Vec::new();
            let mut completion_message: Option<String> = None;

            for call in &calls {
                let call_id = call["id"].as_str().unwrap_or("");
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value =
                    serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));

                if name == "task_complete" {
                    let message = args["message"].as_str().unwrap_or("Done.").to_string();
                    emit_tool_event("task_complete", &message, "done", None);
                    history.push(serde_json::json!({"role":"tool","tool_call_id":call_id,"content":"Task marked complete."}));
                    completion_message = Some(message);
                    continue;
                }

                let tool_msg = match dispatch_tool(name, &args).await {
                    // image_url is not valid inside tool-role messages; collect paths and
                    // inject the screenshots as user messages after all tool results.
                    ToolResult::Screenshot(path) => {
                        screenshot_paths.push(path);
                        serde_json::json!({"role":"tool","tool_call_id":call_id,"content":"Screenshot captured."})
                    }
                    ToolResult::Text(text) => {
                        serde_json::json!({"role":"tool","tool_call_id":call_id,"content":text})
                    }
                };
                history.push(tool_msg);
            }

            if let Some(message) = completion_message {
                return Ok(message);
            }

            // Inject each screenshot as a user message so the model can see the image.
            for path in screenshot_paths {
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let b64 = general_purpose::STANDARD.encode(&bytes);
                        history.push(serde_json::json!({
                            "role": "user",
                            "content": [
                                {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}",b64),"detail":"low"}},
                                {"type":"text","text":"Current screen state. Continue based on what you see."}
                            ]
                        }));
                    }
                    Err(e) => {
                        history.push(serde_json::json!({"role":"user","content":format!("Screenshot read error: {}",e)}));
                    }
                }
            }
        }
    }

    Err("Max turns reached".to_string())
}

// ── Conversation History ──────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallRecord {
    tool: String,
    input: String,
    output: Option<String>,
    status: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationRecord {
    id: String,
    timestamp: u64,
    transcript: String,
    response: String,
    screenshot_path: Option<String>,
    tool_calls: Vec<ToolCallRecord>,
}

#[allow(dead_code)]
struct HistoryState(tokio::sync::Mutex<Vec<ConversationRecord>>);

fn history_file_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| std::env::temp_dir());
    base.join("sharp").join("history.json")
}

fn load_history_from_disk() -> Vec<ConversationRecord> {
    let path = history_file_path();
    if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Vec::new()
    }
}

#[allow(dead_code)]
fn save_history_to_disk(records: &[ConversationRecord]) {
    let path = history_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(records) {
        let _ = std::fs::write(&path, data);
    }
}

#[tauri::command]
async fn save_conversation(
    state: tauri::State<'_, HistoryState>,
    record: ConversationRecord,
) -> Result<(), String> {
    let mut history = state.0.lock().await;
    history.insert(0, record);
    history.truncate(20);
    save_history_to_disk(&history);
    Ok(())
}

#[tauri::command]
async fn get_history(
    state: tauri::State<'_, HistoryState>,
) -> Result<Vec<ConversationRecord>, String> {
    Ok(state.0.lock().await.clone())
}

#[tauri::command]
async fn load_conversation(
    app: tauri::AppHandle,
    state: tauri::State<'_, HistoryState>,
    id: String,
) -> Result<(), String> {
    use tauri::{Emitter, Manager};
    let history = state.0.lock().await;
    if let Some(record) = history.iter().find(|r| r.id == id) {
        if let Some(win) = app.get_webview_window("main") {
            win.emit("load-conversation", record.clone())
                .map_err(|e| e.to_string())?;
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
    Ok(())
}

#[tauri::command]
fn toggle_history_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    if let Some(win) = app.get_webview_window("history") {
        if win.is_visible().unwrap_or(false) {
            win.hide().map_err(|e| e.to_string())
        } else {
            win.show().map_err(|e| e.to_string())?;
            win.set_focus().map_err(|e| e.to_string())
        }
    } else {
        Ok(())
    }
}

#[tauri::command]
fn read_screenshot(path: String) -> Result<String, String> {
    use base64::{engine::general_purpose, Engine as _};
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(general_purpose::STANDARD.encode(&bytes))
}

// ─────────────────────────────────────────────────────────────────────────────

struct ConversationState(tokio::sync::Mutex<Vec<serde_json::Value>>);

#[tauri::command]
async fn agent_chat(
    app: tauri::AppHandle,
    state: tauri::State<'_, ConversationState>,
    message: String,
    screenshot_path: Option<String>,
) -> Result<String, String> {
    use base64::{engine::general_purpose, Engine as _};
    use tauri::Emitter;

    let api_key = std::env::var("OPENAI_API_KEY").map_err(|e| e.to_string())?;
    let tools = tool_schemas();

    let user_content = if let Some(ref path) = screenshot_path {
        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read screenshot: {}", e))?;
        let b64 = general_purpose::STANDARD.encode(&bytes);
        serde_json::json!([
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}",b64),"detail":"low"}},
            {"type":"text","text":message}
        ])
    } else {
        serde_json::json!(message)
    };

    let mut history = state.0.lock().await.clone();
    history.push(serde_json::json!({"role":"user","content":user_content}));

    app.emit("agent-thinking", true).ok();

    let result = TOOL_APP_HANDLE
        .scope(app.clone(), async move {
            run_agent_loop(&api_key, &mut history, &tools, 25)
                .await
                .map(|response| (response, history))
        })
        .await;

    app.emit("agent-thinking", false).ok();

    let (response, updated_history) = result?;
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
        .manage(HistoryState(tokio::sync::Mutex::new(
            load_history_from_disk(),
        )))
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

            let window = app.get_webview_window("main").unwrap();

            // Pin widget to top-center of primary monitor
            if let Ok(Some(monitor)) = window.primary_monitor() {
                let logical_w = monitor.size().width as f64 / monitor.scale_factor();
                let x = (logical_w / 2.0 - 240.0).max(0.0);
                window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                    x,
                    y: 20.0,
                }))?;
            }


            // Create history window (hidden; shown via Ctrl+Shift+H or the widget button)
            let history_win = tauri::WebviewWindowBuilder::new(
                app,
                "history",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("GlideWin — History")
            .inner_size(1200.0, 800.0)
            .resizable(true)
            .skip_taskbar(false)
            .decorations(true)
            .theme(Some(tauri::Theme::Dark))
            .visible(false)
            .build()?;

            // Paint the title bar to match the body background (#0f0f11 = RGB 15,15,17).
            // COLORREF format: 0x00BBGGRR → 0x00110F0F.
            // DWMWA_CAPTION_COLOR (35) requires Windows 11 Build 22000+.
            #[cfg(target_os = "windows")]
            if let Ok(hwnd) = history_win.hwnd() {
                const DWMWA_CAPTION_COLOR: u32 = 35;
                let color: u32 = 0x0011_0F0F;
                unsafe {
                    DwmSetWindowAttribute(
                        hwnd.0,
                        DWMWA_CAPTION_COLOR,
                        &color as *const u32 as *const _,
                        std::mem::size_of::<u32>() as u32,
                    );
                }
            }

            // Register Ctrl+Shift+Space once at the Rust level so React lifecycle
            // (StrictMode double-mount, hot-reload) can never cause "already registered" errors.
            let handle = app.handle().clone();
            app.global_shortcut().on_shortcut(
                "CommandOrControl+Shift+Space",
                move |_app, _shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        use tauri::Emitter;
                        if let Some(win) = handle.get_webview_window("main") {
                            if win.is_visible().unwrap_or(false) {
                                let _ = win.hide();
                            } else {
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
                },
            )?;

            // Register Ctrl+Shift+H to toggle the history window
            let handle_h = app.handle().clone();
            app.global_shortcut().on_shortcut(
                "CommandOrControl+Shift+H",
                move |_app, _shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let handle_h = handle_h.clone();
                    tauri::async_runtime::spawn(async move {
                        use tauri::Manager;
                        if let Some(win) = handle_h.get_webview_window("history") {
                            if win.is_visible().unwrap_or(false) {
                                let _ = win.hide();
                            } else {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    });
                },
            )?;

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
            save_conversation,
            get_history,
            load_conversation,
            toggle_history_window,
            read_screenshot,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
