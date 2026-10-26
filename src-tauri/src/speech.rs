//! 本地离线语音输入。
//!
//! 录音数据只在内存中保留：CPAL 回调把麦克风样本写入有界内存缓冲区，结束录音后在
//! 16 kHz 单声道归一化并直接交给 whisper.cpp。模型是用户显式下载或导入的本地文件；
//! 模型下载使用固定 HTTPS 地址、流式 SHA1 校验和临时文件替换，不会自动联网。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const MODEL_FILE_NAME: &str = "ggml-base.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
// whisper.cpp/models/README.md 给出的 base 多语言模型 SHA1；官方表格没有 SHA256。
const MODEL_SHA1: &str = "465707469ff3a37a2b9b8d8f89f2f99de7299dac";
const MAX_MODEL_BYTES: u64 = 220 * 1024 * 1024;
const MAX_RECORDING_SECONDS: u64 = 60;
const TARGET_SAMPLE_RATE: u32 = 16_000;
const SILENCE_RMS_THRESHOLD: f32 = 0.001;

/// 可序列化的语音模块状态，供 Tauri 命令和前端轮询使用。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpeechStatus {
    pub state: String,
    pub model_available: bool,
    pub progress: Option<u8>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub error: Option<String>,
}

/// 已校验的本地模型信息。模型路径只指向应用数据目录中的副本。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpeechModelInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha1: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording,
    Transcribing,
    Downloading,
    Importing,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Transcribing => "transcribing",
            Self::Downloading => "downloading",
            Self::Importing => "importing",
        }
    }
}

struct Inner {
    phase: Phase,
    recording: Option<Recording>,
    operation_cancel: Option<Arc<AtomicBool>>,
    progress: Option<u8>,
    last_error: Option<String>,
}

/// CPAL 的 Stream 在 0.15.3 中不能 Send/Sync；它始终由 recording_thread 持有，管理器
/// 只跨线程保存停止标志和 JoinHandle，避免把平台音频句柄放进 Arc<Mutex<...>>。
struct Recording {
    capture: Arc<Mutex<CaptureBuffer>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sample_rate: u32,
    channels: u16,
    started: Instant,
}

struct CaptureBuffer {
    samples: Vec<f32>,
    max_samples: usize,
    limit_reached: bool,
    stream_error: Option<String>,
}

struct CaptureConfig {
    sample_rate: u32,
    channels: u16,
}

/// 本地语音管理器。构造不会访问麦克风、读取用户文件或联网；所有外部操作均显式调用。
pub struct SpeechManager {
    data_dir: PathBuf,
    inner: Mutex<Inner>,
}

impl SpeechManager {
    /// 创建线程安全的语音管理器。模型目录会在下载或导入时按需创建。
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                recording: None,
                operation_cancel: None,
                progress: None,
                last_error: None,
            }),
        })
    }

    /// 返回当前状态；只检查模型目标文件元数据，不校验或读取模型内容。
    pub fn status(&self) -> SpeechStatus {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (duration_ms, sample_rate) = match inner.recording.as_ref() {
            Some(recording) if inner.phase == Phase::Recording => (
                Some(
                    recording
                        .started
                        .elapsed()
                        .as_millis()
                        .min(MAX_RECORDING_SECONDS as u128 * 1000) as u64,
                ),
                Some(recording.sample_rate),
            ),
            _ => (None, None),
        };
        SpeechStatus {
            state: inner.phase.as_str().into(),
            model_available: self.model_path().is_file(),
            progress: inner.progress,
            duration_ms,
            sample_rate,
            error: inner.last_error.clone(),
        }
    }

    /// 开始按住录音。录音上限为 60 秒，前端松开按键或失焦后调用 [`finish`] 或 [`cancel`]。
    pub fn start(&self) -> Result<(), String> {
        let model_path = self.model_path();
        let (capture, stop) = {
            let mut inner = self.lock_inner();
            ensure_idle(inner.phase)?;
            if !model_path.is_file() {
                return Err("尚未导入或下载离线语音模型。".into());
            }
            let capture = Arc::new(Mutex::new(CaptureBuffer {
                samples: Vec::new(),
                max_samples: 0,
                limit_reached: false,
                stream_error: None,
            }));
            let stop = Arc::new(AtomicBool::new(false));
            inner.phase = Phase::Recording;
            inner.recording = None;
            inner.operation_cancel = Some(stop.clone());
            inner.progress = None;
            inner.last_error = None;
            (capture, stop)
        };

        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread_capture = capture.clone();
        let thread_stop = stop.clone();
        let recording_thread = thread::spawn(move || {
            recording_worker(thread_capture, thread_stop, ready_sender);
        });

        let ready = ready_receiver.recv_timeout(Duration::from_secs(5));
        let config = match ready {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = recording_thread.join();
                self.fail_operation(&stop, error);
                return Err(self.last_error_or("无法开始录音。"));
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = recording_thread.join();
                let message = format!("录音设备启动超时：{}", error);
                self.fail_operation(&stop, message.clone());
                return Err(message);
            }
        };

        // cancel() 可能在设备初始化期间先执行；此时不把已取消的线程重新登记为活动录音。
        if stop.load(Ordering::Acquire) {
            let _ = recording_thread.join();
            self.finish_operation(&stop, Err("录音已取消。".into()))
        } else {
            let mut inner = self.lock_inner();
            if inner.phase != Phase::Recording
                || inner
                    .operation_cancel
                    .as_ref()
                    .map_or(true, |active| !Arc::ptr_eq(active, &stop))
            {
                drop(inner);
                stop.store(true, Ordering::Release);
                let _ = recording_thread.join();
                return Err("录音已取消。".into());
            }
            inner.recording = Some(Recording {
                capture,
                stop,
                thread: Some(recording_thread),
                sample_rate: config.sample_rate,
                channels: config.channels,
                started: Instant::now(),
            });
            Ok(())
        }
    }

    /// 取消录音、模型下载/导入或正在进行的 whisper 识别。空闲时调用是幂等的。
    pub fn cancel(&self) -> Result<(), String> {
        let recording = {
            let mut inner = self.lock_inner();
            match inner.phase {
                Phase::Idle => return Ok(()),
                Phase::Recording => {
                    if let Some(cancel) = &inner.operation_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    let recording = inner.recording.take();
                    inner.phase = Phase::Idle;
                    inner.operation_cancel = None;
                    inner.progress = None;
                    inner.last_error = None;
                    recording
                }
                Phase::Transcribing | Phase::Downloading | Phase::Importing => {
                    if let Some(cancel) = &inner.operation_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    return Ok(());
                }
            }
        };

        if let Some(recording) = recording {
            stop_recording(recording)?;
        }
        Ok(())
    }

    /// 结束录音并在本机用多语言 base 模型转写，返回纯文本结果。
    pub fn finish(&self) -> Result<String, String> {
        let (recording, cancel) = {
            let mut inner = self.lock_inner();
            if inner.phase != Phase::Recording {
                return Err("当前没有可结束的录音。".into());
            }
            let recording = match inner.recording.take() {
                Some(recording) => recording,
                None => {
                    let error = "录音设备尚未准备完成。".to_owned();
                    if let Some(cancel) = inner.operation_cancel.take() {
                        cancel.store(true, Ordering::Release);
                    }
                    inner.phase = Phase::Idle;
                    inner.progress = None;
                    inner.last_error = Some(error.clone());
                    return Err(error);
                }
            };
            let cancel = Arc::new(AtomicBool::new(false));
            inner.phase = Phase::Transcribing;
            inner.operation_cancel = Some(cancel.clone());
            inner.progress = None;
            (recording, cancel)
        };

        let capture = recording.capture.clone();
        let sample_rate = recording.sample_rate;
        let channels = recording.channels;
        if let Err(error) = stop_recording(recording) {
            return self.finish_operation(&cancel, Err(error));
        }

        let snapshot = {
            let mut buffer = match capture.lock() {
                Ok(buffer) => buffer,
                Err(_) => {
                    return self.finish_operation(&cancel, Err("录音缓冲区状态异常。".to_owned()))
                }
            };
            CaptureSnapshot {
                samples: std::mem::take(&mut buffer.samples),
                sample_rate,
                channels,
                stream_error: buffer.stream_error.take(),
            }
        };
        drop(capture);

        if let Some(error) = snapshot.stream_error {
            return self.finish_operation(&cancel, Err(format!("录音设备已断开：{}", error)));
        }
        // 达到 60 秒时回调只保留前 60 秒样本；结束操作仍可正常转写这段有界音频。
        if cancel.load(Ordering::Acquire) {
            return self.finish_operation(&cancel, Err("语音识别已取消。".into()));
        }

        let audio = match prepare_audio(&snapshot.samples, snapshot.sample_rate, snapshot.channels)
        {
            Ok(audio) => audio,
            Err(error) => return self.finish_operation(&cancel, Err(error)),
        };
        // 到这里后 snapshot.samples 已不再需要；识别只接收 16 kHz 单声道数据。
        drop(snapshot);
        let result = transcribe(&self.model_path(), &audio, &cancel);
        drop(audio);
        self.finish_operation(&cancel, result)
    }

    /// 显式下载官方多语言 base 模型。不会在构造、录音或识别时自动联网。
    pub fn download_model(&self) -> Result<SpeechModelInfo, String> {
        let cancel = self.begin_operation(Phase::Downloading)?;
        let result = self.download_model_inner(&cancel);
        self.finish_operation(&cancel, result)
    }

    /// 显式导入用户选择的 ggml base 模型，读取源文件并校验官方 SHA1 后复制到模型目录。
    pub fn import_model(&self, source: PathBuf) -> Result<SpeechModelInfo, String> {
        let cancel = self.begin_operation(Phase::Importing)?;
        let result = self.import_model_inner(&source, &cancel);
        self.finish_operation(&cancel, result)
    }

    fn download_model_inner(&self, cancel: &Arc<AtomicBool>) -> Result<SpeechModelInfo, String> {
        let destination = self.model_path();
        if verify_model_file(&destination)? {
            self.set_progress(Some(100));
            return model_info(&destination);
        }
        let directory = destination
            .parent()
            .ok_or_else(|| "语音模型目录无效。".to_owned())?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("无法创建语音模型目录：{}", error))?;

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(30))
            .build();
        let response = agent
            .get(MODEL_URL)
            .call()
            .map_err(|error| format!("无法下载离线语音模型：{}", error))?;
        let expected = response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok());
        if expected.is_some_and(|size| size > MAX_MODEL_BYTES) {
            return Err("语音模型下载大小超过安全上限。".into());
        }
        let mut reader = response.into_reader();
        let result = self.write_verified_model(&mut reader, expected, &destination, cancel);
        if result.is_err() {
            let _ = fs::remove_file(partial_path(&destination));
        }
        result
    }

    fn import_model_inner(
        &self,
        source: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<SpeechModelInfo, String> {
        if !source.is_file() {
            return Err("请选择现有的 ggml 语音模型文件。".into());
        }
        let destination = self.model_path();
        if source == destination {
            if verify_model_file(&destination)? {
                self.set_progress(Some(100));
                return model_info(&destination);
            }
            return Err("应用目录中的语音模型校验失败，请重新导入有效模型。".into());
        }
        let mut reader =
            File::open(source).map_err(|error| format!("无法读取语音模型：{}", error))?;
        let size = reader
            .metadata()
            .map_err(|error| format!("无法读取语音模型大小：{}", error))?
            .len();
        if size > MAX_MODEL_BYTES {
            return Err("导入的语音模型超过安全大小上限。".into());
        }
        let directory = destination
            .parent()
            .ok_or_else(|| "语音模型目录无效。".to_owned())?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("无法创建语音模型目录：{}", error))?;
        self.write_verified_model(&mut reader, Some(size), &destination, cancel)
    }

    fn write_verified_model<R: Read>(
        &self,
        reader: &mut R,
        expected_size: Option<u64>,
        destination: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<SpeechModelInfo, String> {
        let partial = partial_path(destination);
        let result =
            self.write_verified_model_inner(reader, expected_size, destination, cancel, &partial);
        // 无论是读取、写入、取消还是校验失败，都不能留下可被下次操作误用的 partial。
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }

    fn write_verified_model_inner<R: Read>(
        &self,
        reader: &mut R,
        expected_size: Option<u64>,
        destination: &Path,
        cancel: &Arc<AtomicBool>,
        partial: &Path,
    ) -> Result<SpeechModelInfo, String> {
        let mut file =
            File::create(&partial).map_err(|error| format!("无法创建临时模型文件：{}", error))?;
        let mut hasher = Sha1::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut written = 0_u64;
        loop {
            if cancel.load(Ordering::Acquire) {
                let _ = fs::remove_file(&partial);
                return Err("语音模型操作已取消。".into());
            }
            let count = reader
                .read(&mut buffer)
                .map_err(|error| format!("读取语音模型失败：{}", error))?;
            if count == 0 {
                break;
            }
            written = written
                .checked_add(count as u64)
                .ok_or_else(|| "语音模型大小溢出。".to_owned())?;
            if written > MAX_MODEL_BYTES {
                let _ = fs::remove_file(&partial);
                return Err("语音模型超过安全大小上限。".into());
            }
            hasher.update(&buffer[..count]);
            file.write_all(&buffer[..count])
                .map_err(|error| format!("写入临时模型文件失败：{}", error))?;
            let progress = expected_size
                .filter(|size| *size > 0)
                .map(|size| ((written.saturating_mul(100) / size).min(100)) as u8);
            self.set_progress(progress);
        }
        file.flush()
            .map_err(|error| format!("刷新临时模型文件失败：{}", error))?;
        drop(file);

        if expected_size.is_some_and(|size| size != written) {
            let _ = fs::remove_file(&partial);
            return Err("语音模型下载内容不完整。".into());
        }
        let digest = format!("{:x}", hasher.finalize());
        if digest != MODEL_SHA1 {
            let _ = fs::remove_file(&partial);
            return Err(format!(
                "语音模型 SHA1 校验失败：收到 {}，期望 {}。",
                digest, MODEL_SHA1
            ));
        }

        // partial 已经完成长度和 SHA1 校验后才发布。Windows 使用系统的替换式移动，
        // 不先删除旧模型，换入失败时仍保留原文件，避免出现“旧模型已删、新模型未到位”的窗口。
        publish_verified_model(&partial, destination)?;
        self.set_progress(Some(100));
        model_info(destination)
    }

    fn model_path(&self) -> PathBuf {
        self.data_dir.join("voice").join(MODEL_FILE_NAME)
    }

    fn begin_operation(&self, phase: Phase) -> Result<Arc<AtomicBool>, String> {
        let mut inner = self.lock_inner();
        ensure_idle(inner.phase)?;
        let cancel = Arc::new(AtomicBool::new(false));
        inner.phase = phase;
        inner.operation_cancel = Some(cancel.clone());
        inner.progress = Some(0);
        inner.last_error = None;
        Ok(cancel)
    }

    fn set_progress(&self, progress: Option<u8>) {
        let mut inner = self.lock_inner();
        inner.progress = progress;
    }

    fn finish_operation<T>(
        &self,
        cancel: &Arc<AtomicBool>,
        result: Result<T, String>,
    ) -> Result<T, String> {
        let mut inner = self.lock_inner();
        if inner
            .operation_cancel
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, cancel))
        {
            inner.phase = Phase::Idle;
            inner.operation_cancel = None;
            inner.progress = None;
            inner.last_error = result.as_ref().err().cloned();
        }
        result
    }

    fn fail_operation(&self, cancel: &Arc<AtomicBool>, error: String) {
        let _ = self.finish_operation::<()>(cancel, Err(error));
    }

    fn last_error_or(&self, fallback: &str) -> String {
        self.lock_inner()
            .last_error
            .clone()
            .unwrap_or_else(|| fallback.into())
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 将已经校验的临时模型发布为正式模型。
///
/// Windows 的标准 `fs::rename` 不能覆盖现有文件；直接删除目标文件会让磁盘在
/// 下一步失败时失去可用模型。`MoveFileExW` 的 `REPLACE_EXISTING` 让系统负责替换，
/// 不需要暴露这个删除窗口。临时文件和正式文件始终位于同一个模型目录中。
fn publish_verified_model(partial: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let to_wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let partial_wide = to_wide(partial);
        let destination_wide = to_wide(destination);
        let replaced = unsafe {
            MoveFileExW(
                partial_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            return Err(format!(
                "无法完成语音模型替换：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        fs::rename(partial, destination).map_err(|error| format!("无法完成语音模型替换：{}", error))
    }
}

fn ensure_idle(phase: Phase) -> Result<(), String> {
    if phase != Phase::Idle {
        return Err("语音模块已有操作正在进行。".into());
    }
    Ok(())
}

fn recording_worker(
    capture: Arc<Mutex<CaptureBuffer>>,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CaptureConfig, String>>,
) {
    let result = (|| {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "找不到可用的麦克风输入设备。".to_owned())?;
        let supported = device
            .default_input_config()
            .map_err(|error| format!("无法读取麦克风配置：{}", error))?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels();
        if sample_rate == 0 || channels == 0 {
            return Err("麦克风返回了无效的采样配置。".into());
        }
        let max_samples = ((sample_rate as u64)
            .saturating_mul(channels as u64)
            .saturating_mul(MAX_RECORDING_SECONDS))
        .min(usize::MAX as u64) as usize;
        {
            let mut buffer = capture
                .lock()
                .map_err(|_| "录音缓冲区状态异常。".to_owned())?;
            buffer.max_samples = max_samples;
            buffer
                .samples
                .reserve(max_samples.min(sample_rate as usize * channels as usize));
        }

        let config: cpal::StreamConfig = supported.clone().into();
        let stream =
            build_input_stream(&device, &config, supported.sample_format(), capture.clone())?;
        stream
            .play()
            .map_err(|error| format!("无法启动麦克风：{}", error))?;
        ready
            .send(Ok(CaptureConfig {
                sample_rate,
                channels,
            }))
            .map_err(|_| "录音状态已被取消。".to_owned())?;

        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(20));
            // 达到 60 秒后停止底层音频流；管理器仍保持 recording 状态，等待前端调用
            // finish() 转写已经收集到的有界音频，或调用 cancel() 丢弃它。
            if capture
                .lock()
                .map(|buffer| buffer.limit_reached)
                .unwrap_or(true)
            {
                break;
            }
        }
        drop(stream);
        Ok(())
    })();

    if let Err(error) = result {
        let _ = ready.send(Err(error));
    }
}

fn build_typed_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    capture: Arc<Mutex<CaptureBuffer>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    let error_capture = capture.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| append_samples(data, &capture),
            move |error| record_stream_error(&error_capture, error.to_string()),
            None,
        )
        .map_err(|error| format!("无法创建麦克风输入流：{}", error))
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: SampleFormat,
    capture: Arc<Mutex<CaptureBuffer>>,
) -> Result<cpal::Stream, String> {
    match format {
        SampleFormat::I8 => build_typed_input_stream::<i8>(device, config, capture),
        SampleFormat::I16 => build_typed_input_stream::<i16>(device, config, capture),
        SampleFormat::I32 => build_typed_input_stream::<i32>(device, config, capture),
        SampleFormat::I64 => build_typed_input_stream::<i64>(device, config, capture),
        SampleFormat::U8 => build_typed_input_stream::<u8>(device, config, capture),
        SampleFormat::U16 => build_typed_input_stream::<u16>(device, config, capture),
        SampleFormat::U32 => build_typed_input_stream::<u32>(device, config, capture),
        SampleFormat::U64 => build_typed_input_stream::<u64>(device, config, capture),
        SampleFormat::F32 => build_typed_input_stream::<f32>(device, config, capture),
        SampleFormat::F64 => build_typed_input_stream::<f64>(device, config, capture),
        other => Err(format!("麦克风采样格式 {} 暂不支持。", other)),
    }
}

fn append_samples<T>(data: &[T], capture: &Arc<Mutex<CaptureBuffer>>)
where
    T: Sample + Copy,
    f32: FromSample<T>,
{
    let Ok(mut buffer) = capture.try_lock() else {
        // 回调绝不能阻塞音频线程；下一次回调会继续填充缓冲区。
        return;
    };
    let remaining = buffer.max_samples.saturating_sub(buffer.samples.len());
    for sample in data.iter().take(remaining) {
        buffer.samples.push(f32::from_sample(*sample));
    }
    if data.len() > remaining {
        buffer.limit_reached = true;
    }
}

fn record_stream_error(capture: &Arc<Mutex<CaptureBuffer>>, error: String) {
    if let Ok(mut buffer) = capture.try_lock() {
        buffer.stream_error = Some(error);
    }
}

fn stop_recording(mut recording: Recording) -> Result<(), String> {
    recording.stop.store(true, Ordering::Release);
    if let Some(thread) = recording.thread.take() {
        thread.join().map_err(|_| "录音线程异常结束。".to_owned())?;
    }
    Ok(())
}

struct CaptureSnapshot {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
    stream_error: Option<String>,
}

fn prepare_audio(samples: &[f32], sample_rate: u32, channels: u16) -> Result<Vec<f32>, String> {
    if sample_rate == 0 || channels == 0 {
        return Err("录音采样配置无效。".into());
    }
    if samples.is_empty() {
        return Err("没有录到音频，请按住录音按钮后再试。".into());
    }
    if samples.len() % channels as usize != 0 {
        return Err("录音数据不完整，无法识别。".into());
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err("录音包含损坏的音频样本。".into());
    }

    let channels = channels as usize;
    let frames = samples.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for frame in samples.chunks_exact(channels) {
        let sum: f32 = frame.iter().copied().sum();
        mono.push(sum / channels as f32);
    }
    let rms = (mono.iter().map(|sample| sample * sample).sum::<f32>() / mono.len() as f32).sqrt();
    if !rms.is_finite() || rms < SILENCE_RMS_THRESHOLD {
        return Err("没有检测到清晰语音，请靠近麦克风后再试。".into());
    }

    if sample_rate == TARGET_SAMPLE_RATE {
        return Ok(mono);
    }
    resample_linear(&mono, sample_rate, TARGET_SAMPLE_RATE)
}

fn resample_linear(
    samples: &[f32],
    source_rate: u32,
    target_rate: u32,
) -> Result<Vec<f32>, String> {
    if samples.is_empty() {
        return Err("没有可识别的音频样本。".into());
    }
    if source_rate == 0 || target_rate == 0 {
        return Err("音频采样率无效。".into());
    }
    let output_len = ((samples.len() as u64)
        .saturating_mul(target_rate as u64)
        .saturating_add(source_rate as u64 / 2)
        / source_rate as u64) as usize;
    if output_len == 0 {
        return Err("音频长度不足以进行识别。".into());
    }
    let ratio = source_rate as f64 / target_rate as f64;
    let last = samples.len() - 1;
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        if left >= last {
            output.push(samples[last]);
            continue;
        }
        let fraction = (position - left as f64) as f32;
        output.push(samples[left] * (1.0 - fraction) + samples[left + 1] * fraction);
    }
    Ok(output)
}

fn transcribe(model: &Path, audio: &[f32], cancel: &Arc<AtomicBool>) -> Result<String, String> {
    if audio.is_empty() {
        return Err("没有可识别的音频。".into());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("语音识别已取消。".into());
    }
    // 模型可能在导入后被外部修改或损坏。进入原生 C++ 解析器前重新核对摘要，
    // 不能只凭文件存在就把任意二进制当成已验证模型；校验不读取或保存录音。
    if !verify_model_file(model)? {
        return Err("本地语音模型缺失或校验失败，请重新下载或导入多语言 base 模型。".into());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("语音识别已取消。".into());
    }
    let context = WhisperContext::new_with_params(model, WhisperContextParameters::default())
        .map_err(|error| format!("无法加载离线语音模型：{}", error))?;
    let mut state = context
        .create_state()
        .map_err(|error| format!("无法创建语音识别状态：{}", error))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 0 });
    params.set_n_threads(
        std::thread::available_parallelism().map_or(1, |count| count.get().min(8) as i32),
    );
    params.set_language(None);
    // None 会让 whisper.cpp 先自动检测语言，再继续完整转写；当前 C API 中将
    // detect_language 设为 true 的含义是“只检测并立即返回”，不能用于本流程。
    // 单次短语音在自动语言检测后仍需保留默认上下文；关闭上下文会让
    // whisper.cpp 的编码阶段在部分中文音频上直接返回失败。
    params.set_no_timestamps(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    // whisper-rs 的安全闭包回调以 FnMut 形式传给 C；whisper.cpp 可能从多个
    // 编码线程同时调用它，实测会在自动语言检测后的编码阶段返回 -6。这里
    // 传入只读 AtomicBool 的 C 函数指针，避免闭包对象的并发别名问题。
    let cancel_ptr = Arc::as_ptr(cancel) as *mut c_void;
    unsafe {
        params.set_abort_callback(Some(abort_if_cancelled));
        params.set_abort_callback_user_data(cancel_ptr);
    }

    state
        .full(params, audio)
        .map_err(|error| format!("离线语音识别失败：{}", error))?;
    if cancel.load(Ordering::Acquire) {
        return Err("语音识别已取消。".into());
    }
    let mut text = String::new();
    for segment in state.as_iter() {
        text.push_str(&segment.to_string());
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("没有识别到清晰语音。".into());
    }
    Ok(text)
}

unsafe extern "C" fn abort_if_cancelled(user_data: *mut c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    // `user_data` 在 transcribe 调用期间由调用方持有 Arc，指向的 AtomicBool
    // 不会被释放；回调只执行原子读，不修改 whisper.cpp 的状态。
    (&*(user_data as *const AtomicBool)).load(Ordering::Acquire)
}

fn verify_model_file(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let metadata =
        fs::metadata(path).map_err(|error| format!("无法读取语音模型大小：{}", error))?;
    if metadata.len() == 0 || metadata.len() > MAX_MODEL_BYTES {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(|error| format!("无法读取语音模型：{}", error))?;
    let digest = sha1_reader(&mut file)?;
    Ok(digest == MODEL_SHA1)
}

fn sha1_reader<R: Read>(reader: &mut R) -> Result<String, String> {
    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("读取模型校验失败：{}", error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn model_info(path: &Path) -> Result<SpeechModelInfo, String> {
    let size_bytes = fs::metadata(path)
        .map_err(|error| format!("无法读取语音模型元数据：{}", error))?
        .len();
    Ok(SpeechModelInfo {
        name: MODEL_FILE_NAME.into(),
        path: path.to_string_lossy().into_owned(),
        size_bytes,
        sha1: MODEL_SHA1.into(),
    })
}

fn partial_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.partial", path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_stereo_audio_to_whisper_rate() -> Result<(), String> {
        let samples = vec![0.0, 0.0, 1.0, 1.0, 0.0, 0.0, -1.0, -1.0];
        let audio = prepare_audio(&samples, 8_000, 2)?;
        assert_eq!(audio.len(), 8);
        assert!(audio.iter().any(|sample| *sample > 0.5));
        assert!(audio.iter().any(|sample| *sample < -0.5));
        Ok(())
    }

    #[test]
    fn rejects_empty_silent_and_corrupt_audio() {
        assert!(prepare_audio(&[], TARGET_SAMPLE_RATE, 1).is_err());
        assert!(prepare_audio(&[0.0; 160], TARGET_SAMPLE_RATE, 1).is_err());
        assert!(prepare_audio(&[f32::NAN], TARGET_SAMPLE_RATE, 1).is_err());
    }

    #[test]
    fn manager_is_idle_without_model_or_microphone_access() {
        let directory = tempfile::tempdir().expect("创建测试目录");
        let manager = SpeechManager::new(directory.path().join("data"));
        let status = manager.status();
        assert_eq!(status.state, "idle");
        assert!(!status.model_available);
        assert!(manager.cancel().is_ok());
    }

    #[test]
    fn failed_model_import_removes_partial_file() {
        let directory = tempfile::tempdir().expect("创建测试目录");
        let source = directory.path().join("invalid-model.bin");
        fs::write(&source, b"not a whisper model").expect("写入测试模型");
        let data_dir = directory.path().join("data");
        let manager = SpeechManager::new(data_dir.clone());

        assert!(manager.import_model(source).is_err());
        let model = data_dir.join("voice").join(MODEL_FILE_NAME);
        assert!(!model.exists());
        assert!(!partial_path(&model).exists());
        assert_eq!(manager.status().state, "idle");
    }
}
