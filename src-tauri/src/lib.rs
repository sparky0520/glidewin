use std::sync::{Arc, Mutex};

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn capture_screen() -> Result<String, String> {
    use xcap::Monitor;
    use std::time::{SystemTime, UNIX_EPOCH};

    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let monitor = monitors.first().ok_or("No monitors found")?;
    let image = monitor.capture_image().map_err(|e| e.to_string())?;

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let filename = format!("glidewin_capture_{}.png", timestamp);
    let path = std::env::temp_dir().join(filename);
    image.save(&path).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

// --- Microphone Recording ---

struct RecordingHandle {
    stop_signal: Arc<Mutex<bool>>,
    thread_handle: Option<std::thread::JoinHandle<Result<(), String>>>,
    file_path: String,
}

struct RecorderState(Mutex<Option<RecordingHandle>>);

#[tauri::command]
fn start_recording(state: tauri::State<'_, RecorderState>) -> Result<String, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("Already recording".into());
    }

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let filename = format!("glidewin_recording_{}.wav", timestamp);
    let path = std::env::temp_dir().join(&filename);
    let file_path = path.to_string_lossy().into_owned();

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
    let writer_clone = writer.clone();
    let stop_clone = stop_signal.clone();
    let file_path_clone = file_path.clone();

    let thread_handle = std::thread::spawn(move || -> Result<(), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No microphone found")?;
        let supported_config = device.default_input_config().map_err(|e| e.to_string())?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let writer_for_cb = writer_clone.clone();

        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if let Ok(mut w) = writer_for_cb.lock() {
                        if let Some(ref mut writer) = *w {
                            for &sample in data { let _ = writer.write_sample(sample); }
                        }
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
                            for &sample in data {
                                let _ = writer.write_sample((sample * i16::MAX as f32) as i16);
                            }
                        }
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

    *guard = Some(RecordingHandle { stop_signal, thread_handle: Some(thread_handle), file_path: file_path_clone });
    Ok(file_path)
}

#[tauri::command]
fn stop_recording(state: tauri::State<'_, RecorderState>) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    let handle = guard.take().ok_or("Not currently recording")?;
    *handle.stop_signal.lock().unwrap() = true;
    if let Some(thread) = handle.thread_handle {
        thread.join().map_err(|_| "Recording thread panicked".to_string())??;
    }
    Ok(handle.file_path)
}

// --- Speech Transcription ---

#[tauri::command]
async fn transcribe_audio(file_path: String) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let file_bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read audio file: {}", e))?;

    let file_name = std::path::Path::new(&file_path)
        .file_name().unwrap_or_default().to_string_lossy().into_owned();

    let part = reqwest::multipart::Part::bytes(file_bytes)
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

#[tauri::command]
async fn speak_text(text: String) -> Result<(), String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(&api_key)
        .json(&serde_json::json!({"model": "tts-1", "input": text, "voice": "alloy", "response_format": "mp3"}))
        .send().await
        .map_err(|e| format!("TTS request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("TTS API error ({}): {}", status, body));
    }

    let audio_bytes = response.bytes().await
        .map_err(|e| format!("Failed to read audio: {}", e))?.to_vec();

    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        use rodio::{Decoder, OutputStream, Sink};
        use std::io::Cursor;

        let (_stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| format!("Audio output error: {}", e))?;
        let sink = Sink::try_new(&stream_handle)
            .map_err(|e| format!("Sink error: {}", e))?;
        let source = Decoder::new(Cursor::new(audio_bytes))
            .map_err(|e| format!("Decode error: {}", e))?;
        sink.append(source);
        sink.sleep_until_end();
        Ok(())
    }).await.map_err(|e| format!("Playback error: {}", e))?
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

    if !output.status.success() && !stderr.is_empty() {
        let err = stderr.trim().to_string();
        emit_tool_event("run_powershell", &command, "error", Some(&err));
        return Err(ToolError::ToolCallError(err.into()));
    }

    let result = if stdout.trim().is_empty() {
        "Done (no output).".to_string()
    } else {
        stdout.trim().to_string()
    };

    emit_tool_event("run_powershell", &command, "done", Some(&result));
    Ok(result)
}

/// Open an application, file, or URL on Windows using the shell's default handler.
#[rig_tool]
async fn open_app(
    /// Name or path of the app/file/URL to open (e.g. "notepad", "chrome", "https://example.com")
    target: String,
) -> Result<String, ToolError> {
    emit_tool_event("open_app", &target, "start", None);

    let result = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            &format!("Start-Process '{}'", target.replace('\'', "''"))])
        .output()
        .await;

    match result {
        Ok(_) => {
            let msg = format!("Opened: {}", target);
            emit_tool_event("open_app", &target, "done", Some(&msg));
            Ok(msg)
        }
        Err(e) => {
            emit_tool_event("open_app", &target, "error", Some(&e.to_string()));
            Err(ToolError::ToolCallError(e.to_string().into()))
        }
    }
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
             You have tools to run PowerShell commands and open applications. \
             Always tell the user what you are about to do before calling a tool. \
             Keep responses concise. Never delete files or make destructive changes \
             without explicit user confirmation.",
        )
        .max_tokens(2048)
        .default_max_turns(10)
        .tool(RunPowershell)
        .tool(OpenApp)
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
