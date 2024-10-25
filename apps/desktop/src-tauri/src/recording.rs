use crate::flags;
use cap_media::{encoders::*, feeds::*, filters::*, pipeline::*, sources::*, MediaError};
use cap_project::CursorEvent;
use device_query::{DeviceQuery, DeviceState};
use serde::Serialize;
use specta::Type;
use std::fs::File;
use std::sync::Arc;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{path::PathBuf, time::Duration};
use tokio::sync::{oneshot, Mutex};

use objc::rc::autoreleasepool;
use objc::runtime::{Class, Object, Sel, BOOL, YES};
use objc::*;

use crate::RecordingOptions;

// TODO: Hacky, please fix
pub const FPS: u32 = 30;

#[tauri::command(async)]
#[specta::specta]
pub fn list_capture_screens() -> Vec<CaptureScreen> {
    ScreenCaptureSource::list_screens()
}

#[tauri::command(async)]
#[specta::specta]
pub fn list_capture_windows() -> Vec<CaptureWindow> {
    ScreenCaptureSource::list_targets()
}

#[tauri::command(async)]
#[specta::specta]
pub fn list_cameras() -> Vec<String> {
    CameraFeed::list_cameras()
}

#[derive(Type, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InProgressRecording {
    pub recording_dir: PathBuf,
    #[serde(skip)]
    pub pipeline: Pipeline<SynchronisedClock<()>>,
    #[serde(skip)]
    pub display_output_path: PathBuf,
    #[serde(skip)]
    pub camera_output_path: Option<PathBuf>,
    #[serde(skip)]
    pub audio_output_path: Option<PathBuf>,
    pub display_source: ScreenCaptureTarget,
    pub segments: Vec<f64>,
    #[serde(skip)]
    pub cursor_moves: oneshot::Receiver<Vec<CursorEvent>>,
    #[serde(skip)]
    pub cursor_clicks: oneshot::Receiver<Vec<CursorEvent>>,
    #[serde(skip)]
    pub stop_signal: Arc<Mutex<bool>>,
}

unsafe impl Send for InProgressRecording {}
unsafe impl Sync for InProgressRecording {}

pub struct CompletedRecording {
    pub recording_dir: PathBuf,
    pub display_output_path: PathBuf,
    pub camera_output_path: Option<PathBuf>,
    pub audio_output_path: Option<PathBuf>,
    pub display_source: ScreenCaptureTarget,
    pub segments: Vec<f64>,
}

impl InProgressRecording {
    pub async fn stop(mut self) -> CompletedRecording {
        use cap_project::*;

        let meta = RecordingMeta {
            project_path: self.recording_dir.clone(),
            sharing: None,
            pretty_name: format!(
                "Cap {}",
                chrono::Local::now().format("%Y-%m-%d at %H.%M.%S")
            ),
            display: Display {
                path: self
                    .display_output_path
                    .strip_prefix(&self.recording_dir)
                    .unwrap()
                    .to_owned(),
            },
            camera: self.camera_output_path.as_ref().map(|path| CameraMeta {
                path: path.strip_prefix(&self.recording_dir).unwrap().to_owned(),
            }),
            audio: self.audio_output_path.as_ref().map(|path| AudioMeta {
                path: path.strip_prefix(&self.recording_dir).unwrap().to_owned(),
            }),
            segments: {
                let relative_segments = self
                    .segments
                    .iter()
                    .map(|s| s - self.segments[0])
                    .collect::<Vec<_>>();

                let mut segments = vec![];

                let mut diff = 0.0;

                for (i, chunk) in relative_segments.chunks_exact(2).enumerate() {
                    if i < relative_segments.len() / 2 {
                        segments.push(RecordingSegment {
                            start: diff,
                            end: chunk[1] - chunk[0] + diff,
                        });
                    }

                    diff += chunk[1] - chunk[0];
                }

                segments
            },
            cursor: Some(PathBuf::from("cursor.json")),
        };

        // Signal the mouse event tracking to stop
        if let Err(error) = self.pipeline.shutdown().await {
            eprintln!("Error while stopping recording: {error}");
        }

        *self.stop_signal.lock().await = true;

        if flags::RECORD_MOUSE {
            // Save mouse events to files

            let mut file = File::create(self.recording_dir.join("cursor.json")).unwrap();
            serde_json::to_writer(
                &mut file,
                &CursorData {
                    clicks: self.cursor_clicks.await.unwrap(),
                    moves: self.cursor_moves.await.unwrap(),
                },
            )
            .unwrap();
        }

        meta.save_for_project();

        CompletedRecording {
            recording_dir: self.recording_dir,
            display_output_path: self.display_output_path,
            camera_output_path: self.camera_output_path,
            audio_output_path: self.audio_output_path,
            display_source: self.display_source,
            segments: self.segments,
        }
    }

    pub async fn stop_and_discard(&mut self) {
        // Signal the mouse event tracking to stop
        if let Err(error) = self.pipeline.shutdown().await {
            eprintln!("Error while stopping recording: {error}");
        }

        // Delete all recorded files
        if let Err(e) = std::fs::remove_dir_all(&self.recording_dir) {
            eprintln!("Failed to delete recording directory: {:?}", e);
        }
    }

    pub async fn pause(&mut self) -> Result<(), String> {
        let _ = self.pipeline.pause().await;
        Ok(())
    }

    pub async fn play(&mut self) -> Result<(), String> {
        let _ = self.pipeline.play().await;
        Ok(())
    }
}

pub async fn start(
    recording_dir: PathBuf,
    recording_options: &RecordingOptions,
    camera_feed: Option<&CameraFeed>,
) -> Result<InProgressRecording, MediaError> {
    let content_dir = recording_dir.join("content");

    std::fs::create_dir_all(&content_dir).unwrap();

    let clock = SynchronisedClock::<()>::new();
    let mut pipeline_builder = Pipeline::builder(clock);

    let display_output_path = content_dir.join("display.mp4");
    let mut audio_output_path = None;
    let mut camera_output_path = None;

    let screen_source =
        ScreenCaptureSource::init(dbg!(&recording_options.capture_target), None, None);
    let screen_config = screen_source.info();
    let screen_bounds = screen_source.bounds.clone();

    let output_config = screen_config.scaled(1920, 30);
    let screen_filter = VideoFilter::init("screen", screen_config, output_config)?;
    let screen_encoder = H264Encoder::init(
        "screen",
        output_config,
        Output::File(display_output_path.clone()),
    )?;
    pipeline_builder = pipeline_builder
        .source("screen_capture", screen_source)
        .pipe("screen_capture_filter", screen_filter)
        .sink("screen_capture_encoder", screen_encoder);

    if let Some(mic_source) = AudioInputSource::init(recording_options.audio_input_name.as_ref()) {
        let mic_config = mic_source.info();
        audio_output_path = Some(content_dir.join("audio-input.mp3"));

        // let mic_filter = AudioFilter::init("microphone", mic_config, "aresample=async=1:min_hard_comp=0.100000:first_pts=0")?;
        let mic_encoder = MP3Encoder::init(
            "microphone",
            mic_config,
            Output::File(audio_output_path.clone().unwrap()),
        )?;

        pipeline_builder = pipeline_builder
            .source("microphone_capture", mic_source)
            // .pipe("microphone_filter", mic_filter)
            .sink("microphone_encoder", mic_encoder);
    }

    if let Some(camera_source) = CameraSource::init(camera_feed) {
        let camera_config = camera_source.info();
        let output_config = camera_config.scaled(1920, 30);
        camera_output_path = Some(content_dir.join("camera.mp4"));

        let camera_filter = VideoFilter::init("camera", camera_config, output_config)?;
        let camera_encoder = H264Encoder::init(
            "camera",
            output_config,
            Output::File(camera_output_path.clone().unwrap()),
        )?;

        pipeline_builder = pipeline_builder
            .source("camera_capture", camera_source)
            .pipe("camera_filter", camera_filter)
            .sink("camera_encoder", camera_encoder);
    }

    let mut pipeline = pipeline_builder.build().await?;
    pipeline.play().await?;

    let stop_signal = Arc::new(Mutex::new(false));

    let (mouse_moves, mouse_clicks) = {
        let (move_tx, move_rx) = oneshot::channel();
        let (click_tx, click_rx) = oneshot::channel();

        let stop_signal = stop_signal.clone();
        tokio::spawn(async move {
            let device_state = DeviceState::new();
            let mut last_mouse_state = device_state.get_mouse();
            let start_time = Instant::now();

            let mut moves = vec![];
            let mut clicks = vec![];

            while !*stop_signal.lock().await {
                let mouse_state = device_state.get_mouse();
                let elapsed = start_time.elapsed().as_secs_f64() * 1000.0;
                let unix_time = chrono::Utc::now().timestamp_millis() as f64;

                if mouse_state.coords != last_mouse_state.coords {
                    let mouse_event = CursorEvent {
                        active_modifiers: vec![],
                        cursor_id: get_cursor_id(),
                        process_time_ms: elapsed,
                        unix_time_ms: unix_time,
                        x: (mouse_state.coords.0 as f64 - screen_bounds.x) / screen_bounds.width,
                        y: (mouse_state.coords.1 as f64 - screen_bounds.y) / screen_bounds.height,
                    };
                    moves.push(mouse_event);
                }

                if mouse_state.button_pressed[0] && !last_mouse_state.button_pressed[0] {
                    let mouse_event = CursorEvent {
                        active_modifiers: vec![],
                        cursor_id: get_cursor_id(),
                        process_time_ms: elapsed,
                        unix_time_ms: unix_time,
                        x: (mouse_state.coords.0 as f64 - screen_bounds.x) / screen_bounds.width,
                        y: (mouse_state.coords.1 as f64 - screen_bounds.y) / screen_bounds.height,
                    };
                    clicks.push(mouse_event);
                }

                last_mouse_state = mouse_state;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            move_tx.send(moves).unwrap();
            click_tx.send(clicks).unwrap();
        });

        (move_rx, click_rx)
    };

    Ok(InProgressRecording {
        segments: vec![SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()],
        pipeline,
        recording_dir,
        display_source: recording_options.capture_target.clone(),
        display_output_path,
        audio_output_path,
        camera_output_path,
        cursor_moves: mouse_moves,
        cursor_clicks: mouse_clicks,
        stop_signal,
    })
}

fn get_cursor_id() -> String {
    autoreleasepool(|| {
        // Get the NSCursor class
        let nscursor_class = match Class::get("NSCursor") {
            Some(cls) => cls,
            None => return "Unknown".to_string(),
        };

        unsafe {
            // Get the current cursor
            let current_cursor: *mut Object = msg_send![nscursor_class, currentSystemCursor];
            if current_cursor.is_null() {
                return "Unknown".to_string();
            }

            // Define an array of known cursor names
            let cursor_names = [
                "arrowCursor",
                "IBeamCursor",
                "crosshairCursor",
                "closedHandCursor",
                "openHandCursor",
                "pointingHandCursor",
                "resizeLeftCursor",
                "resizeRightCursor",
                "resizeLeftRightCursor",
                "resizeUpCursor",
                "resizeDownCursor",
                "resizeUpDownCursor",
                "disappearingItemCursor",
                "IBeamCursorForVerticalLayout",
                "operationNotAllowedCursor",
                "dragLinkCursor",
                "dragCopyCursor",
                "contextualMenuCursor",
            ];

            // Iterate through known cursor names
            for cursor_name in cursor_names.iter() {
                let sel = Sel::register(cursor_name);
                let cursor: *mut Object = msg_send![nscursor_class, performSelector:sel];
                if !cursor.is_null() {
                    let is_equal: BOOL = msg_send![current_cursor, isEqual:cursor];
                    if is_equal == YES {
                        return cursor_name.to_string();
                    }
                }
            }

            // If no match is found, return "Unknown"
            "Unknown".to_string()
        }
    })
}
