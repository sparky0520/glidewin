// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
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
    /// Signal to stop the recording thread
    stop_signal: Arc<Mutex<bool>>,
    /// Thread handle for the recording thread
    thread_handle: Option<std::thread::JoinHandle<Result<(), String>>>,
    /// Path to the output WAV file
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

    // Prepare output path
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let filename = format!("glidewin_recording_{}.wav", timestamp);
    let path = std::env::temp_dir().join(&filename);
    let file_path = path.to_string_lossy().into_owned();

    // Get default input device and config
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("No microphone found")?;
    let supported_config = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels();

    // Create WAV writer matching the device's native format
    let spec = hound::WavSpec {
        channels: channels,
        sample_rate: sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let writer = hound::WavWriter::create(&path, spec).map_err(|e| e.to_string())?;
    let writer = Arc::new(Mutex::new(Some(writer)));
    let stop_signal = Arc::new(Mutex::new(false));

    // Clone for the recording thread
    let writer_clone = writer.clone();
    let stop_clone = stop_signal.clone();
    let file_path_clone = file_path.clone();

    // Spawn recording on a dedicated thread (cpal::Stream is !Send)
    let thread_handle = std::thread::spawn(move || -> Result<(), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No microphone found")?;
        let supported_config = device.default_input_config().map_err(|e| e.to_string())?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();

        let writer_for_cb = writer_clone.clone();

        let stream = match sample_format {
            cpal::SampleFormat::I16 => {
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut w) = writer_for_cb.lock() {
                            if let Some(ref mut writer) = *w {
                                for &sample in data {
                                    let _ = writer.write_sample(sample);
                                }
                            }
                        }
                    },
                    |err| eprintln!("Audio stream error: {}", err),
                    None,
                ).map_err(|e| e.to_string())?
            },
            cpal::SampleFormat::F32 => {
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut w) = writer_for_cb.lock() {
                            if let Some(ref mut writer) = *w {
                                for &sample in data {
                                    // Convert f32 [-1.0, 1.0] to i16
                                    let sample_i16 = (sample * i16::MAX as f32) as i16;
                                    let _ = writer.write_sample(sample_i16);
                                }
                            }
                        }
                    },
                    |err| eprintln!("Audio stream error: {}", err),
                    None,
                ).map_err(|e| e.to_string())?
            },
            _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
        };

        stream.play().map_err(|e| e.to_string())?;

        // Wait until stop signal is set
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if *stop_clone.lock().unwrap() {
                break;
            }
        }

        // Drop stream to stop recording
        drop(stream);

        // Finalize WAV file
        if let Ok(mut w) = writer_clone.lock() {
            if let Some(writer) = w.take() {
                writer.finalize().map_err(|e| e.to_string())?;
            }
        }

        Ok(())
    });

    *guard = Some(RecordingHandle {
        stop_signal,
        thread_handle: Some(thread_handle),
        file_path: file_path_clone,
    });

    Ok(file_path)
}

#[tauri::command]
fn stop_recording(state: tauri::State<'_, RecorderState>) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    let handle = guard.take().ok_or("Not currently recording")?;

    // Signal the recording thread to stop
    *handle.stop_signal.lock().unwrap() = true;

    // Wait for the thread to finish
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
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let part = reqwest::multipart::Part::bytes(file_bytes)
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

    let text = json["text"]
        .as_str()
        .ok_or("No 'text' field in API response")?
        .to_string();

    Ok(text)
}

// --- GPT Integration ---

#[tauri::command]
async fn ask_gpt(screenshot_path: String, transcript: String) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose};

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let image_bytes = std::fs::read(&screenshot_path)
        .map_err(|e| format!("Failed to read screenshot: {}", e))?;

    let base64_image = general_purpose::STANDARD.encode(&image_bytes);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": { "url": format!("data:image/png;base64,{}", base64_image) }
                },
                {
                    "type": "text",
                    "text": transcript
                }
            ]
        }],
        "max_tokens": 1024
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

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("No content in API response")?
        .to_string();

    Ok(text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    dotenvy::dotenv().ok();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .manage(RecorderState(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![greet, capture_screen, start_recording, stop_recording, transcribe_audio, ask_gpt])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
