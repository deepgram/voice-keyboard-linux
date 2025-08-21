use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use http::{header::AUTHORIZATION, HeaderValue};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info};

pub const STT_URL: &str = "wss://api.preview.deepgram.com/v2/listen";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WordInfo {
    pub word: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub event: String,
    pub turn_index: u32,
    pub start: f64,
    pub timestamp: f64,
    pub transcript: String,
    pub words: Vec<WordInfo>,
    pub end_of_turn_confidence: f64,
}

// New server message schema with `type` discriminator
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum ServerMessage {
    Connected {
        request_id: String,
        sequence_id: u32,
    },
    TurnInfo {
        request_id: String,
        sequence_id: u32,
        event: String,
        turn_index: u32,
        audio_window_start: f64,
        audio_window_end: f64,
        transcript: String,
        words: Vec<WordInfo>,
        end_of_turn_confidence: f64,
    },
    Error {
        sequence_id: u32,
        code: String,
        description: String,
    },
    // Configuration ack/echo; fields are optional or not used here
    Configuration {
        #[serde(default)]
        eot_threshold: Option<f64>,
        #[serde(default)]
        preflight_threshold: Option<f64>,
    },
}

pub struct SttClient {
    url: String,
    sample_rate: u32,
}

impl SttClient {
    pub fn new(url: &str, sample_rate: u32) -> Self {
        Self {
            url: url.to_string(),
            sample_rate,
        }
    }

    pub async fn connect_and_transcribe<F>(
        &self,
        mut on_transcription: F,
    ) -> Result<(mpsc::Sender<Vec<u8>>, tokio::task::JoinHandle<Result<()>>)>
    where
        F: FnMut(TranscriptionResult) + Send + 'static,
    {
        // Build WebSocket URL with query parameters
        let ws_url = format!(
            "{}?model=flux-general-en&sample_rate={}&encoding=linear16",
            self.url, self.sample_rate
        );

        debug!("Connecting to speech-to-text service: {}", ws_url);

        // Build request (allows setting headers)
        let mut request = ws_url
            .into_client_request()
            .context("Failed to build websocket client request")?;

        // Optional Authorization from environment
        if let Ok(api_key) = env::var("DEEPGRAM_API_KEY") {
            if !api_key.is_empty() {
                let value = format!("Token {api_key}");
                match HeaderValue::from_str(&value) {
                    Ok(hv) => {
                        request.headers_mut().insert(AUTHORIZATION, hv);
                        debug!("Added Authorization header from DEEPGRAM_API_KEY");
                    }
                    Err(_) => {
                        // Treat invalid header as fatal
                        bail!(
                            "Invalid Authorization header value constructed from DEEPGRAM_API_KEY"
                        );
                    }
                }
            }
        } else {
            debug!("DEEPGRAM_API_KEY not set; connecting without Authorization header");
        }

        // Establish WebSocket connection with the request
        let (ws_stream, _) = connect_async(request)
            .await
            .context("Failed to connect to WebSocket")?;

        debug!("Connected to speech-to-text service");

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Create channel for sending audio data
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);

        // Spawn task to handle WebSocket communication
        let handle = tokio::spawn(async move {
            // Task to send audio data (fatal on send error)
            let send_task = tokio::spawn(async move {
                while let Some(audio_data) = audio_rx.recv().await {
                    if let Err(e) = ws_sender.send(Message::Binary(audio_data)).await {
                        error!("Failed to send audio data: {}", e);
                        return Err(anyhow!("failed to send audio over websocket: {e}"));
                    }
                }

                // Audio channel closed: inform server no more audio is coming
                let close_msg = String::from("{\"type\":\"CloseStream\"}");
                debug!("Sending CloseStream control message");
                ws_sender
                    .send(Message::Text(close_msg))
                    .await
                    .map_err(|e| anyhow!("failed to send CloseStream: {e}"))?;

                // Do not close the socket from client; server will close after sending responses
                Ok::<(), anyhow::Error>(())
            });

            // Task to receive messages (fatal on parse/socket error per policy)
            let receive_task = tokio::spawn(async move {
                while let Some(msg) = ws_receiver.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            debug!("Received text message: {}", text);

                            // Parse by `type`
                            let parsed: ServerMessage = match serde_json::from_str(&text) {
                                Ok(m) => m,
                                Err(e) => {
                                    error!("Failed to parse message JSON: {} in {}", e, text);
                                    return Err(anyhow!("invalid server JSON: {e}"));
                                }
                            };

                            match parsed {
                                ServerMessage::Connected {
                                    request_id,
                                    sequence_id,
                                } => {
                                    info!(
                                        "Connected: request_id={}, sequence_id={}",
                                        request_id, sequence_id
                                    );
                                }
                                ServerMessage::Configuration {
                                    eot_threshold,
                                    preflight_threshold,
                                } => {
                                    info!("Configuration ack: eot_threshold={:?}, preflight_threshold={:?}", eot_threshold, preflight_threshold);
                                }
                                ServerMessage::Error {
                                    sequence_id,
                                    code,
                                    description,
                                } => {
                                    error!("Server error [{}]: {}", code, description);
                                    return Err(anyhow!(
                                        "server error (seq {}): {} - {}",
                                        sequence_id,
                                        code,
                                        description
                                    ));
                                }
                                ServerMessage::TurnInfo {
                                    request_id: _,
                                    sequence_id: _,
                                    event,
                                    turn_index,
                                    audio_window_start,
                                    audio_window_end,
                                    transcript,
                                    words,
                                    end_of_turn_confidence,
                                } => {
                                    // Map to callback struct
                                    let result = TranscriptionResult {
                                        event,
                                        turn_index,
                                        start: audio_window_start,
                                        timestamp: audio_window_end,
                                        transcript,
                                        words,
                                        end_of_turn_confidence,
                                    };
                                    on_transcription(result);
                                }
                            }
                        }
                        Ok(Message::Binary(_data)) => {
                            return Err(anyhow!("received binary data--this isn't expected"))
                        }
                        Ok(Message::Close(_)) => {
                            debug!("WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            error!("WebSocket error: {}", e);
                            return Err(anyhow!("websocket receive error: {e}"));
                        }
                        _ => {}
                    }
                }
                Ok::<(), anyhow::Error>(())
            });

            // Wait for both tasks to finish
            let (_sr, _rr) = tokio::try_join!(send_task, receive_task)?;

            Ok(())
        });

        Ok((audio_tx, handle))
    }
}

pub struct AudioBuffer {
    buffer: Vec<u8>,
    chunk_size: usize,
}

impl AudioBuffer {
    pub fn new(sample_rate: u32, chunk_duration_ms: u32) -> Self {
        // Calculate chunk size for 16-bit PCM audio
        // chunk_size = sample_rate * (chunk_duration_ms / 1000) * 2 bytes per sample
        let chunk_size = (sample_rate * chunk_duration_ms / 1000 * 2) as usize;

        debug!(
            "AudioBuffer: sample_rate={}, chunk_duration_ms={}, calculated chunk_size={} bytes",
            sample_rate, chunk_duration_ms, chunk_size
        );

        Self {
            buffer: Vec::new(),
            chunk_size,
        }
    }

    pub fn add_samples(&mut self, samples: &[f32]) -> Vec<Vec<u8>> {
        // Convert f32 samples to 16-bit PCM
        let pcm_data: Vec<u8> = samples
            .iter()
            .flat_map(|&sample| {
                let pcm_sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                pcm_sample.to_le_bytes()
            })
            .collect();

        self.buffer.extend_from_slice(&pcm_data);

        // Extract complete chunks
        let mut chunks = Vec::new();
        while self.buffer.len() >= self.chunk_size {
            let chunk: Vec<u8> = self.buffer.drain(..self.chunk_size).collect();
            chunks.push(chunk);
        }

        if !chunks.is_empty() {
            debug!(
                "Created {} audio chunks of {} bytes each (buffer size: {}, chunk_size: {})",
                chunks.len(),
                chunks[0].len(),
                self.buffer.len(),
                self.chunk_size
            );
        }

        chunks
    }

    #[allow(dead_code)]
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if !self.buffer.is_empty() {
            let remaining = self.buffer.drain(..).collect();
            Some(remaining)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    fn init_tracing() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    #[tokio::test]
    async fn test_connect_and_receive_turninfo_with_silence() {
        init_tracing();
        // Allow overriding URL via env; default to the new preview endpoint the app uses
        let stt_url = std::env::var("STT_TEST_URL").unwrap_or_else(|_| STT_URL.to_string());
        let sample_rate: u32 = 16_000;

        let client = SttClient::new(&stt_url, sample_rate);

        // Flag flipped when we successfully deserialize a TurnInfo and invoke callback
        let got_result = Arc::new(AtomicBool::new(false));
        let got_result_clone = got_result.clone();

        let (audio_tx, _handle) = client
            .connect_and_transcribe(move |_result| {
                // We only need to know that deserialization worked and callback fired
                got_result_clone.store(true, Ordering::SeqCst);
            })
            .await
            .expect("failed to connect to STT service");

        // Stream silence in 80 ms chunks at real-time rate until we get a result (with max duration)
        let chunk_ms: u32 = 80;
        let samples_per_chunk: usize = (sample_rate as usize * chunk_ms as usize) / 1000; // 16k * 80ms = 1280
        let bytes_per_chunk: usize = samples_per_chunk * 2; // PCM16
        let zeros = vec![0u8; bytes_per_chunk];

        // Safety timeout so we don't hang CI forever
        let stream_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut audio_tx = Some(audio_tx);
        loop {
            if got_result.load(Ordering::SeqCst) {
                break;
            }
            if tokio::time::Instant::now() > stream_deadline {
                break;
            }

            if let Some(tx) = &audio_tx {
                tx.send(zeros.clone()).await.expect("audio send failed");
            }
            tokio::time::sleep(Duration::from_millis(chunk_ms as u64)).await;
        }

        // Drop the sender to signal end-of-audio; client will send CloseStream and await server close
        if let Some(tx) = audio_tx.take() {
            drop(tx);
        }

        assert!(
            got_result.load(Ordering::SeqCst),
            "timed out waiting for transcription result"
        );
    }

    #[tokio::test]
    async fn test_stream_silence_until_response() {
        init_tracing();
        // Allow overriding URL via env; default to the new preview endpoint the app uses
        let stt_url = std::env::var("STT_TEST_URL").unwrap_or_else(|_| STT_URL.to_string());
        let sample_rate: u32 = 16_000;

        let client = SttClient::new(&stt_url, sample_rate);

        // Flag flipped when we successfully deserialize a TurnInfo and invoke callback
        let got_result = Arc::new(AtomicBool::new(false));
        let got_result_clone = got_result.clone();

        let (audio_tx, _handle) = client
            .connect_and_transcribe(move |_result| {
                got_result_clone.store(true, Ordering::SeqCst);
            })
            .await
            .expect("failed to connect to STT service");

        // Stream silence continuously in 80 ms chunks at real-time rate until we get a result
        let chunk_ms: u32 = 80;
        let samples_per_chunk: usize = (sample_rate as usize * chunk_ms as usize) / 1000; // 1280 samples
        let bytes_per_chunk: usize = samples_per_chunk * 2; // PCM16
        let zeros = vec![0u8; bytes_per_chunk];

        // Max stream duration to avoid hanging the test forever
        let stream_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let audio_tx = Some(audio_tx);
        loop {
            if got_result.load(Ordering::SeqCst) {
                break;
            }
            if tokio::time::Instant::now() > stream_deadline {
                break;
            }

            if let Some(tx) = &audio_tx {
                tx.send(zeros.clone()).await.expect("audio send failed");
            }
            tokio::time::sleep(Duration::from_millis(chunk_ms as u64)).await;
        }

        assert!(
            got_result.load(Ordering::SeqCst),
            "did not receive any TurnInfo within the allowed time"
        );
    }
}
