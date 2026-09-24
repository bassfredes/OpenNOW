//! Explicit local experiment; regular builds and 4:2:0 keep Media Foundation.
use super::decoder::{DecodedVideoFrame, DecoderDevice, MfDecoder};
use crate::{
    BackendEvent, EncodedVideoFrame, VideoChromaFormat, VideoCodec, VideoColorMatrix,
    VideoColorPrimaries, VideoFormat, VideoPixelFormat, VideoTransferFunction, WindowsDecoderMode,
};
use crate::{aperture::VideoAperture, queue::BoundedQueue};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_Y410, DXGI_SAMPLE_DESC,
};
use ::windows::core::Interface;
use std::{collections::VecDeque, ffi::c_void, fs::OpenOptions, io::Write, ptr::NonNull};

#[repr(C)]
#[derive(Default)]
pub(super) struct FrameInfo {
    pub(super) pts: i64,
    pub(super) duration: i64,
    pub(super) range: i32,
    pub(super) primaries: i32,
    pub(super) transfer: i32,
    pub(super) matrix: i32,
    pub(super) pixel_format: i32,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) source_depth: i32,
    pub(super) output_layout: i32,
}
unsafe extern "C" {
    fn on_hevc_keyframe_layout(data: *const u8, size: i32, width: i32, height: i32) -> i32;
    fn on_nvdec_pixel_format_name(format: i32) -> *const std::ffi::c_char;
    fn on_nvdec_open(width: i32, height: i32, depth: i32) -> *mut c_void;
    fn on_nvdec_close(decoder: *mut c_void);
    fn on_nvdec_send(
        decoder: *mut c_void,
        data: *const u8,
        size: i32,
        pts: i64,
        duration: i64,
        key: i32,
    ) -> i32;
    fn on_nvdec_drain(decoder: *mut c_void) -> i32;
    fn on_nvdec_reset(decoder: *mut c_void);
    fn on_nvdec_receive(
        decoder: *mut c_void,
        pixels: *mut u32,
        count: usize,
        info: *mut FrameInfo,
    ) -> i32;
}

#[cfg(test)]
unsafe extern "C" {
    fn on_nvdec_y410_shift(av_pix_fmt: i32) -> i32;
    fn av_get_pix_fmt(name: *const std::ffi::c_char) -> i32;
}

#[cfg(feature = "nvdec-gpu-interop")]
unsafe extern "C" {
    fn on_nvdec_enable_gpu(decoder: *mut c_void, device: *mut c_void) -> i32;
    fn on_nvdec_receive_gpu(
        decoder: *mut c_void,
        info: *mut FrameInfo,
        slot: *mut *mut c_void,
    ) -> i32;
    fn on_nvdec_gpu_texture(slot: *mut c_void, plane: i32) -> *mut c_void;
    fn on_nvdec_gpu_release(slot: *mut c_void);
}

#[cfg(feature = "nvdec-gpu-interop")]
pub(super) struct GpuPlanes {
    slot: NonNull<c_void>,
    pub(super) textures: [Option<::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>; 3],
}
// C slots use atomic leases and retain their CUDA device context independently
// of the decoder. Drop pushes that context on the releasing thread. D3D11's
// adopted immediate context is multithread-protected by the existing owner.
#[cfg(feature = "nvdec-gpu-interop")]
unsafe impl Send for GpuPlanes {}
#[cfg(feature = "nvdec-gpu-interop")]
unsafe impl Sync for GpuPlanes {}
#[cfg(feature = "nvdec-gpu-interop")]
impl GpuPlanes {
    unsafe fn from_slot(slot: NonNull<c_void>) -> Self {
        use ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
        let textures = std::array::from_fn(|p| {
            let raw = unsafe { on_nvdec_gpu_texture(slot.as_ptr(), p as i32) };
            if raw.is_null() {
                return None;
            }
            let borrowed = std::mem::ManuallyDrop::new(unsafe { ID3D11Texture2D::from_raw(raw) });
            Some((*borrowed).clone())
        });
        Self { slot, textures }
    }
}
#[cfg(feature = "nvdec-gpu-interop")]
impl Drop for GpuPlanes {
    fn drop(&mut self) {
        unsafe { on_nvdec_gpu_release(self.slot.as_ptr()) };
    }
}

pub(super) enum Decoder {
    Mf(MfDecoder),
    Nv(NvDecoder),
    D3d11va(super::d3d11va::D3d11vaDecoder),
}
impl Decoder {
    pub(super) fn new<G: DecoderDevice>(
        g: &G,
        format: VideoFormat,
        mode: WindowsDecoderMode,
    ) -> Result<Self, String> {
        if std::env::var("OPENNOW_EXPERIMENTAL_NVDEC444").as_deref() == Ok("1")
            && format.codec == VideoCodec::H265
            && matches!(
                format.pixel_format,
                VideoPixelFormat::Ayuv | VideoPixelFormat::Y410
            )
        {
            // The live embedded decoder takes the same zero-copy D3D11VA
            // 4:4:4 route the Settings probe validated; NVDEC CPU-transfer is
            // only a fallback when the driver route is unavailable or fails
            // at open time.
            match super::d3d11va::D3d11vaDecoder::new(g, format, mode) {
                Ok(decoder) => {
                    video_log!(
                        "decoder route=D3D11VA 4:4:4 zero-copy {}x{}",
                        format.width,
                        format.height
                    );
                    return Ok(Self::D3d11va(decoder));
                }
                Err(message) => {
                    video_log!("decoder route=NVDEC 4:4:4 fallback: {message}");
                }
            }
            return NvDecoder::new(g, format).map(Self::Nv);
        }
        MfDecoder::new(g, format, mode).map(Self::Mf)
    }
    pub(super) fn probe<G: DecoderDevice>(
        g: &G,
        codec: VideoCodec,
        mode: WindowsDecoderMode,
    ) -> Result<(), String> {
        MfDecoder::probe(g, codec, mode)
    }
    pub(super) fn wants_input(&self) -> bool {
        match self {
            Self::Mf(d) => d.wants_input(),
            Self::Nv(d) => !d.stopped && d.credit && d.timestamps.len() < 16,
            Self::D3d11va(d) => d.wants_input(),
        }
    }
    pub(super) fn format(&self) -> VideoFormat {
        match self {
            Self::Mf(d) => d.format(),
            Self::Nv(d) => d.format,
            Self::D3d11va(d) => d.format(),
        }
    }
    pub(super) fn stop(&mut self) {
        match self {
            Self::Mf(d) => d.stop(),
            Self::Nv(d) => d.stop(),
            Self::D3d11va(d) => d.stop(),
        }
    }
    pub(super) fn reset_at_keyframe(&mut self) -> bool {
        match self {
            Self::D3d11va(d) => d.reset_at_keyframe(),
            Self::Mf(_) => false,
            Self::Nv(d) => {
                if d.stopped {
                    return false;
                }
                let started = std::time::Instant::now();
                unsafe { on_nvdec_reset(d.handle.as_ptr()) };
                d.timestamps.clear();
                d.credit = true;
                video_log!(
                    "NVDEC keyframe reset retained CUDA context elapsed_us={}",
                    started.elapsed().as_micros()
                );
                true
            }
        }
    }
    pub(super) fn submit(&mut self, frame: EncodedVideoFrame) -> Result<(), String> {
        dump_received_access_unit(&frame);
        match self {
            Self::Mf(d) => d.submit(frame),
            Self::Nv(d) => d.submit(frame),
            Self::D3d11va(d) => d.submit(frame),
        }
    }
    // The request may name a color quality the seat did not encode. Choose
    // the decoder only from the parsed bitstream, before submitting this
    // unmodified random-access unit: a valid SPS whose depth or chroma
    // differs from the request always routes to the decoder that matches the
    // actual stream instead of failing on it.
    pub(super) fn select_bitstream_decoder<G: DecoderDevice>(
        &mut self,
        device: &G,
        requested: VideoFormat,
        mode: WindowsDecoderMode,
        frame: &EncodedVideoFrame,
    ) -> Result<bool, String> {
        let Some(actual) = keyframe_format(requested, frame) else {
            return Ok(false);
        };
        let Some(target) = plan_bitstream_decoder(self.format(), actual) else {
            return Ok(false);
        };
        let mut routed_d3d11va = false;
        let replacement = match target {
            BitstreamDecoder::MediaFoundation => Self::Mf(MfDecoder::new(device, actual, mode)?),
            BitstreamDecoder::FullChroma => {
                match super::d3d11va::D3d11vaDecoder::new(device, actual, mode) {
                    Ok(decoder) => {
                        routed_d3d11va = true;
                        Self::D3d11va(decoder)
                    }
                    Err(message) => {
                        video_log!(
                            "D3D11VA 4:4:4 route unavailable, falling back to NVDEC full-chroma: {message}"
                        );
                        Self::Nv(NvDecoder::new(device, actual)?)
                    }
                }
            }
        };
        video_log!(
            "HEVC bitstream decoder selection: requested={:?} actual={:?} depth={} route={}; request and compressed pixels unchanged",
            requested.pixel_format,
            actual.pixel_format,
            actual.pixel_format.bit_depth(),
            match (target, routed_d3d11va) {
                (BitstreamDecoder::MediaFoundation, _) => "Media Foundation D3D11",
                (BitstreamDecoder::FullChroma, true) => "D3D11VA Y410/AYUV zero-copy",
                (BitstreamDecoder::FullChroma, false) => "NVDEC full-chroma",
            }
        );
        self.stop();
        *self = replacement;
        Ok(true)
    }
    pub(super) fn poll_output(
        &mut self,
        frames: &mut VecDeque<DecodedVideoFrame>,
        events: &BoundedQueue<BackendEvent>,
    ) -> Result<usize, String> {
        match self {
            Self::Mf(d) => d.poll_output(frames, events),
            Self::Nv(d) => {
                let previous = d.format;
                let produced = d.poll(frames)?;
                if d.format != previous {
                    let _ = events.push(BackendEvent::VideoFormatChanged(d.format));
                }
                Ok(produced)
            }
            Self::D3d11va(d) => {
                let previous = d.format();
                let produced = d.poll(frames)?;
                if d.format() != previous {
                    let _ = events.push(BackendEvent::VideoFormatChanged(d.format()));
                }
                Ok(produced)
            }
        }
    }
    pub(super) fn probe_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        match self {
            Self::Mf(d) => d.probe_frame(data),
            Self::D3d11va(d) => d.probe_frame(data),
            Self::Nv(d) => {
                d.submit(EncodedVideoFrame {
                    codec: VideoCodec::H265,
                    data: data.to_vec(),
                    timestamp_100ns: 0,
                    duration_100ns: d.format.frame_duration_100ns(),
                    key_frame: true,
                    reset_decoder: false,
                })?;
                let result = unsafe { on_nvdec_drain(d.handle.as_ptr()) };
                if result < 0 {
                    return Err(format!("NVDEC probe drain failed: {result}"));
                }
                let mut frames = VecDeque::new();
                d.poll(&mut frames)?;
                frames
                    .pop_front()
                    .ok_or_else(|| "NVDEC probe returned no frame".into())
            }
        }
    }
}

/// `OPENNOW_DUMP_HEVC=<path>` (off unless set): append every access unit the
/// live decoder receives as raw Annex-B, so a real seat stream can be saved
/// during a session and replayed or diffed offline. The environment is read
/// per frame on purpose — no cached state a test could poison.
fn dump_received_access_unit(frame: &EncodedVideoFrame) {
    let Some(path) = std::env::var_os("OPENNOW_DUMP_HEVC").filter(|path| !path.is_empty()) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(&frame.data);
    }
}

/// Which decoder presents a parsed bitstream layout: Media Foundation's
/// D3D11 MFT negotiates NV12/P010 output for 4:2:0; real 4:4:4 uses the
/// zero-copy D3D11VA route when the driver supports it and falls back to the
/// NVDEC CPU-transfer path otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BitstreamDecoder {
    MediaFoundation,
    FullChroma,
}

/// Rebuild the running decoder when its surfaces cannot present `actual`;
/// `None` keeps it. Pixel format and chroma are the decoder's whole shape —
/// they carry both the bit depth and the surface layout — so equality on
/// both means the running decoder already matches what the seat encoded.
fn plan_bitstream_decoder(running: VideoFormat, actual: VideoFormat) -> Option<BitstreamDecoder> {
    if running.pixel_format == actual.pixel_format && running.chroma_format == actual.chroma_format
    {
        return None;
    }
    Some(match actual.chroma_format {
        VideoChromaFormat::Cs420 => BitstreamDecoder::MediaFoundation,
        VideoChromaFormat::Cs444 => BitstreamDecoder::FullChroma,
    })
}

fn keyframe_format(requested: VideoFormat, frame: &EncodedVideoFrame) -> Option<VideoFormat> {
    if requested.codec != VideoCodec::H265
        || frame.codec != VideoCodec::H265
        || !frame.key_frame
        || frame.data.is_empty()
        || frame.data.len() > 32 * 1024 * 1024
    {
        return None;
    }
    let layout = unsafe {
        on_hevc_keyframe_layout(
            frame.data.as_ptr(),
            frame.data.len() as i32,
            requested.width as i32,
            requested.height as i32,
        )
    };
    let (pixel_format, chroma_format) = match layout {
        0 => (VideoPixelFormat::Ayuv, VideoChromaFormat::Cs444),
        1 => (VideoPixelFormat::Y410, VideoChromaFormat::Cs444),
        2 => (VideoPixelFormat::Nv12, VideoChromaFormat::Cs420),
        3 => (VideoPixelFormat::P010, VideoChromaFormat::Cs420),
        _ => return None,
    };
    Some(VideoFormat {
        pixel_format,
        chroma_format,
        // An 8-bit bitstream cannot carry HDR10 and validate() rejects an
        // HDR label on 8-bit decoder output; the actual depth defines the
        // transfer label so the routed decoder always constructs.
        transfer_function: if pixel_format.bit_depth() == 8 {
            VideoTransferFunction::Sdr
        } else {
            requested.transfer_function
        },
        ..requested
    })
}

pub(super) struct NvDecoder {
    handle: NonNull<c_void>,
    device: ID3D11Device,
    format: VideoFormat,
    pixels: Vec<u32>,
    timestamps: VecDeque<(i64, i64)>,
    stopped: bool,
    credit: bool,
    #[cfg(feature = "nvdec-gpu-interop")]
    gpu_planes: bool,
}
impl NvDecoder {
    fn new<G: DecoderDevice>(g: &G, format: VideoFormat) -> Result<Self, String> {
        format.validate().map_err(|e| e.to_string())?;
        let manager = g.device_manager();
        let device = unsafe {
            let h = manager.OpenDeviceHandle().map_err(|e| e.to_string())?;
            let mut raw = std::ptr::null_mut();
            let result = manager.LockDevice(h, &ID3D11Device::IID, &mut raw, true);
            if result.is_ok() {
                let _ = manager.UnlockDevice(h, false);
            }
            let _ = manager.CloseDeviceHandle(h);
            result.map_err(|e| format!("NVDEC bridge get D3D11 device: {e}"))?;
            if raw.is_null() {
                return Err("NVDEC bridge received null D3D11 device".into());
            }
            ID3D11Device::from_raw(raw)
        };
        let handle = NonNull::new(unsafe {
            on_nvdec_open(
                format.width as i32,
                format.height as i32,
                format.pixel_format.bit_depth() as i32,
            )
        })
        .ok_or("Could not initialize experimental NVIDIA HEVC decoder")?;
        #[cfg(feature = "nvdec-gpu-interop")]
        let gpu_planes = format.pixel_format.bit_depth() == 10
            && std::env::var("OPENNOW_NVDEC_GPU_PLANES").as_deref() == Ok("1")
            && std::env::var("OPENNOW_NVDEC_LOW_LATENCY").as_deref() == Ok("1");
        #[cfg(not(feature = "nvdec-gpu-interop"))]
        let gpu_planes = false;
        #[cfg(feature = "nvdec-gpu-interop")]
        if gpu_planes && unsafe { on_nvdec_enable_gpu(handle.as_ptr(), device.as_raw()) } != 0 {
            unsafe { on_nvdec_close(handle.as_ptr()) };
            return Err("Could not enable CUDA/D3D11 plane interoperability".into());
        }
        video_log!(
            "EXPERIMENTAL NVDEC444 enabled: {}x{} depth={} {}; hardware decoder={} gpu=0",
            format.width,
            format.height,
            format.pixel_format.bit_depth(),
            if gpu_planes {
                "CUDA/D3D11 GPU-plane-copy"
            } else {
                "CPU-transfer/D3D11-upload"
            },
            if std::env::var("OPENNOW_NVDEC_LOW_LATENCY").as_deref() == Ok("1") {
                "hevc+cuda complete-access-unit"
            } else {
                "hevc_cuvid"
            }
        );
        Ok(Self {
            handle,
            device,
            format,
            pixels: if gpu_planes {
                Vec::new()
            } else {
                vec![0; format.width as usize * format.height as usize]
            },
            timestamps: VecDeque::with_capacity(16),
            stopped: false,
            credit: true,
            #[cfg(feature = "nvdec-gpu-interop")]
            gpu_planes,
        })
    }
    fn submit(&mut self, frame: EncodedVideoFrame) -> Result<(), String> {
        if self.stopped
            || !self.credit
            || self.timestamps.len() >= 16
            || frame.codec != VideoCodec::H265
            || frame.data.is_empty()
            || frame.data.len() > 32 * 1024 * 1024
        {
            return Err("NVDEC bridge rejected input state/size/codec".into());
        }
        let result = unsafe {
            on_nvdec_send(
                self.handle.as_ptr(),
                frame.data.as_ptr(),
                frame.data.len() as i32,
                frame.timestamp_100ns,
                frame.duration_100ns,
                i32::from(frame.key_frame),
            )
        };
        if result < 0 {
            return Err(format!("NVDEC send failed: {result}"));
        }
        self.timestamps
            .push_back((frame.timestamp_100ns, frame.duration_100ns));
        self.credit = false;
        Ok(())
    }
    fn poll(&mut self, frames: &mut VecDeque<DecodedVideoFrame>) -> Result<usize, String> {
        if self.stopped {
            return Ok(0);
        }
        let mut info = FrameInfo::default();
        #[cfg(feature = "nvdec-gpu-interop")]
        let mut gpu_frame = None;
        let result = unsafe {
            #[cfg(feature = "nvdec-gpu-interop")]
            if self.gpu_planes {
                let mut slot = std::ptr::null_mut();
                let result = on_nvdec_receive_gpu(self.handle.as_ptr(), &mut info, &mut slot);
                gpu_frame = NonNull::new(slot).map(|slot| GpuPlanes::from_slot(slot));
                result
            } else {
                on_nvdec_receive(
                    self.handle.as_ptr(),
                    self.pixels.as_mut_ptr(),
                    self.pixels.len(),
                    &mut info,
                )
            }
            #[cfg(not(feature = "nvdec-gpu-interop"))]
            on_nvdec_receive(
                self.handle.as_ptr(),
                self.pixels.as_mut_ptr(),
                self.pixels.len(),
                &mut info,
            )
        };
        if result < 0 {
            let name = unsafe { on_nvdec_pixel_format_name(info.pixel_format) };
            let name = if name.is_null() {
                "unknown".into()
            } else {
                unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy()
            };
            return Err(format!(
                "NVDEC receive/format validation failed: {result}; requested={:?} actual={name} sourceDepth={} size={}x{} range={} primaries={} transfer={} matrix={}",
                self.format.pixel_format,
                info.source_depth,
                info.width,
                info.height,
                info.range,
                info.primaries,
                info.transfer,
                info.matrix
            ));
        }
        self.credit = true;
        if result == 0 {
            return Ok(0);
        }
        let position = self
            .timestamps
            .iter()
            .position(|(pts, _)| *pts == info.pts)
            .ok_or("NVDEC output timestamp does not match submitted input")?;
        let (_, duration) = self.timestamps.remove(position).unwrap();
        // A stalled consumer may lease every bounded GPU slot. The C bridge
        // consumes and discards that frame without overwriting any live surface.
        if result == 2 {
            return Ok(0);
        }
        // Game exit can switch HDR gameplay back to the host's SDR desktop.
        // Report supported source metadata per frame so the existing renderer
        // reconfigures; never relabel SDR pixels as PQ or enter a reset loop.
        let updated = decoded_color(self.format, &info)?;
        if updated != self.format {
            video_log!(
                "NVDEC decoded color transition: {:?} -> {:?}",
                self.format.transfer_function,
                updated.transfer_function
            );
            self.format = updated;
        }
        let (pixel_format, chroma_format, dxgi_format, bytes_per_row) = match info.output_layout {
            0 => (
                VideoPixelFormat::Ayuv,
                VideoChromaFormat::Cs444,
                DXGI_FORMAT_AYUV,
                self.format.width * 4,
            ),
            1 => (
                VideoPixelFormat::Y410,
                VideoChromaFormat::Cs444,
                DXGI_FORMAT_Y410,
                self.format.width * 4,
            ),
            2 => (
                VideoPixelFormat::Nv12,
                VideoChromaFormat::Cs420,
                DXGI_FORMAT_NV12,
                self.format.width,
            ),
            3 => (
                VideoPixelFormat::P010,
                VideoChromaFormat::Cs420,
                DXGI_FORMAT_P010,
                self.format.width * 2,
            ),
            _ => return Err("NVDEC returned an unknown output layout".into()),
        };
        if self.format.pixel_format != pixel_format {
            video_log!(
                "NVDEC actual decoded format: {:?} -> {:?}, chroma={:?}; reporting server output without upsampling",
                self.format.pixel_format,
                pixel_format,
                chroma_format
            );
        }
        self.format.pixel_format = pixel_format;
        self.format.chroma_format = chroma_format;
        self.format.validate().map_err(|e| e.to_string())?;
        #[cfg(feature = "nvdec-gpu-interop")]
        if let Some(planes) = gpu_frame {
            if frames.len() >= crate::ADAPTIVE_VIDEO_QUEUE_CAPACITY {
                return Err("NVDEC output queue limit reached".into());
            }
            let texture = planes.textures[0]
                .as_ref()
                .ok_or("Missing GPU luma plane")?
                .clone();
            frames.push_back(DecodedVideoFrame {
                format: self.format,
                aperture: VideoAperture::new(self.format.width, self.format.height, None)?,
                texture,
                subresource: 0,
                timestamp_100ns: info.pts,
                duration_100ns: duration,
                _sample: None,
                gpu_planes: Some(planes),
                _lease: None,
            });
            return Ok(1);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.format.width,
            Height: self.format.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: self.pixels.as_ptr().cast(),
            SysMemPitch: bytes_per_row,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
        }
        .map_err(|e| format!("NVDEC D3D11 upload failed: {e}"))?;
        if frames.len() >= crate::ADAPTIVE_VIDEO_QUEUE_CAPACITY {
            return Err("NVDEC output queue limit reached".into());
        }
        frames.push_back(DecodedVideoFrame {
            format: self.format,
            aperture: VideoAperture::new(self.format.width, self.format.height, None)?,
            texture: texture.ok_or("NVDEC upload produced no texture")?,
            subresource: 0,
            timestamp_100ns: info.pts,
            duration_100ns: duration,
            _sample: None,
            #[cfg(feature = "nvdec-gpu-interop")]
            gpu_planes: None,
            #[cfg(feature = "nvdec-experiment")]
            _lease: None,
        });
        Ok(1)
    }
    fn stop(&mut self) {
        if !self.stopped {
            unsafe { on_nvdec_close(self.handle.as_ptr()) };
            self.stopped = true;
            self.timestamps.clear();
        }
    }
}
impl Drop for NvDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn decoded_color(
    mut format: VideoFormat,
    info: &FrameInfo,
) -> Result<VideoFormat, String> {
    format.full_range = match info.range {
        0 => format.full_range,
        1 => false,
        2 => true,
        value => return Err(format!("Unsupported NVDEC color range: {value}")),
    };
    format.color_primaries = match info.primaries {
        2 => format.color_primaries,
        1 => VideoColorPrimaries::Bt709,
        9 => VideoColorPrimaries::Bt2020,
        value => return Err(format!("Unsupported NVDEC color primaries: {value}")),
    };
    format.color_matrix = match info.matrix {
        2 => format.color_matrix,
        1 => VideoColorMatrix::Bt709,
        5 | 6 => VideoColorMatrix::Bt601,
        9 => VideoColorMatrix::Bt2020,
        value => return Err(format!("Unsupported NVDEC color matrix: {value}")),
    };
    format.transfer_function = match info.transfer {
        2 => format.transfer_function,
        1 => VideoTransferFunction::Sdr,
        16 => VideoTransferFunction::Pq,
        18 => VideoTransferFunction::Hlg,
        value => return Err(format!("Unsupported NVDEC transfer function: {value}")),
    };
    format.validate_color().map_err(|error| error.to_string())?;
    Ok(format)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OPENNOW_DUMP_HEVC` must stay off by default and, when set, append
    /// the raw Annex-B access units in order so a live seat stream can be
    /// replayed offline.
    #[test]
    fn opennow_dump_hevc_appends_raw_access_units_only_when_enabled() {
        let path = std::env::temp_dir().join(format!(
            "opennow-dump-test-{}-{:#x}.hevc",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let previous = std::env::var_os("OPENNOW_DUMP_HEVC");
        unsafe { std::env::remove_var("OPENNOW_DUMP_HEVC") };
        let unit = |bytes: &[u8]| EncodedVideoFrame {
            codec: VideoCodec::H265,
            data: bytes.to_vec(),
            timestamp_100ns: 0,
            duration_100ns: 1,
            key_frame: true,
            reset_decoder: false,
        };
        let first = [0, 0, 0, 1, 0x40, 1, 0xAA];
        let second = [0, 0, 1, 0x02, 0x01, 0xBB, 0xCC];
        dump_received_access_unit(&unit(&first));
        assert!(
            !path.exists(),
            "the dump must stay off when OPENNOW_DUMP_HEVC is unset"
        );
        unsafe { std::env::set_var("OPENNOW_DUMP_HEVC", &path) };
        dump_received_access_unit(&unit(&first));
        dump_received_access_unit(&unit(&second));
        let written = std::fs::read(&path).expect("dump file written");
        let mut expected = first.to_vec();
        expected.extend_from_slice(&second);
        assert_eq!(written, expected, "raw Annex-B units appended in order");
        match previous {
            Some(value) => unsafe { std::env::set_var("OPENNOW_DUMP_HEVC", &value) },
            None => unsafe { std::env::remove_var("OPENNOW_DUMP_HEVC") },
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Sample values for the Y410 conversion shift: MSB (left)-aligned
    /// formats — including the rebuilt FFmpeg's `yuv444p10msb` /
    /// `yuv444p12msb` outputs — keep the code in the high bits, while the
    /// LSB-aligned `yuv444p10le` needs no shift.
    #[test]
    fn nvdec_y410_shift_reproduces_sample_values_for_msb_and_lsb_formats() {
        let msb10 = unsafe { av_get_pix_fmt(c"yuv444p10msb".as_ptr()) };
        let msb12 = unsafe { av_get_pix_fmt(c"yuv444p12msb".as_ptr()) };
        let lsb10 = unsafe { av_get_pix_fmt(c"yuv444p10le".as_ptr()) };
        let packed16 = unsafe { av_get_pix_fmt(c"yuv444p16le".as_ptr()) };
        let missing = unsafe { av_get_pix_fmt(c"no-such-pixel-format".as_ptr()) };
        assert_ne!(msb10, missing, "FFmpeg must provide yuv444p10msb");
        assert_ne!(msb12, missing, "FFmpeg must provide yuv444p12msb");
        assert_ne!(lsb10, missing, "FFmpeg must provide yuv444p10le");
        assert_ne!(packed16, missing, "FFmpeg must provide yuv444p16le");
        let (shift_msb10, shift_msb12, shift_lsb10, shift_packed16) = unsafe {
            (
                on_nvdec_y410_shift(msb10),
                on_nvdec_y410_shift(msb12),
                on_nvdec_y410_shift(lsb10),
                on_nvdec_y410_shift(packed16),
            )
        };
        assert_eq!(shift_msb10, 6, "MSB 10-bit codes live at code << 6");
        assert_eq!(shift_msb12, 6, "MSB 12-bit words downconvert with >> 6");
        assert_eq!(shift_lsb10, 0, "LSB 10-bit codes need no shift");
        assert_eq!(
            shift_packed16, 6,
            "cuvid packed16 keeps the historical shift"
        );
        // Stored words as the decoders produce them, and the Y410 codes they
        // must yield after the shift.
        let msb10_word: u16 = 876 << 6;
        let msb12_word: u16 = 876 << 4;
        let lsb10_word: u16 = 876;
        let packed16_word: u16 = 876 << 6;
        assert_eq!((msb10_word >> shift_msb10) as u32, 876);
        assert_eq!((msb12_word >> shift_msb12) as u32, 876 >> 2);
        assert_eq!((lsb10_word >> shift_lsb10) as u32, 876);
        assert_eq!((packed16_word >> shift_packed16) as u32, 876);
    }

    #[test]
    fn hevc_keyframe_layout_follows_the_bitstream_and_rejects_other_resolutions() {
        let samples: &[(&[u8], i32, i32, i32)] = &[
            (
                include_bytes!("../../fixtures/probe/hevc-p010-pq.hevc"),
                1920,
                1080,
                3,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-p010-5k-pq.hevc"),
                5120,
                2880,
                3,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-y410-pq-precision.hevc"),
                1920,
                1080,
                1,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-ayuv-sdr.hevc"),
                1920,
                1080,
                0,
            ),
        ];
        for &(data, width, height, layout) in samples {
            let inspect = |bytes: &[u8], w| unsafe {
                on_hevc_keyframe_layout(bytes.as_ptr(), bytes.len() as i32, w, height)
            };
            // The parser's own pixel format decides depth and chroma, exactly
            // as the bitstream encodes them.
            assert_eq!(inspect(data, width), layout);
            assert_eq!(
                inspect(data, width + 2),
                -1,
                "do not reroute a different resolution"
            );
            assert_eq!(
                inspect(&data[..8], width),
                -1,
                "incomplete headers prove no layout"
            );
            assert_eq!(inspect(&[], width), -1);
        }
    }

    fn requested_hdr_444_format() -> VideoFormat {
        VideoFormat {
            codec: VideoCodec::H265,
            width: 1920,
            height: 1080,
            frame_rate_numerator: std::num::NonZeroU32::new(60).unwrap(),
            frame_rate_denominator: std::num::NonZeroU32::new(1).unwrap(),
            average_bitrate: 20_000_000,
            pixel_format: VideoPixelFormat::Y410,
            chroma_format: VideoChromaFormat::Cs444,
            chroma_siting: crate::VideoChromaSiting::Left,
            full_range: false,
            transfer_function: VideoTransferFunction::Pq,
            color_primaries: VideoColorPrimaries::Bt2020,
            color_matrix: VideoColorMatrix::Bt2020,
        }
    }

    #[test]
    fn eight_bit_420_sdr_routes_away_from_requested_10bit_444_hdr() {
        let requested = requested_hdr_444_format();
        requested
            .validate()
            .expect("the 10-bit 4:4:4 HDR request is itself valid");
        // The seat encoded 8-bit 4:2:0 SDR although 10-bit 4:4:4 HDR was
        // requested — the 2026-09-24 live session that presented no frame.
        let actual = VideoFormat {
            pixel_format: VideoPixelFormat::Nv12,
            chroma_format: VideoChromaFormat::Cs420,
            transfer_function: VideoTransferFunction::Sdr,
            ..requested
        };
        assert_eq!(
            plan_bitstream_decoder(requested, actual),
            Some(BitstreamDecoder::MediaFoundation),
            "8-bit 4:2:0 must move to the Media Foundation NV12 decoder"
        );
        actual
            .validate()
            .expect("the routed 8-bit SDR format must construct");
        // Once the running decoder matches the stream, it is never reinitialized.
        assert_eq!(plan_bitstream_decoder(actual, actual), None);
    }

    #[test]
    fn eight_bit_keyframe_parses_and_labels_sdr_despite_a_ten_bit_hdr_request() {
        let requested = requested_hdr_444_format();
        let frame = EncodedVideoFrame {
            codec: VideoCodec::H265,
            data: include_bytes!("../../fixtures/probe/hevc-ayuv-sdr.hevc").to_vec(),
            timestamp_100ns: 0,
            duration_100ns: requested.frame_duration_100ns(),
            key_frame: true,
            reset_decoder: false,
        };
        let actual = keyframe_format(requested, &frame)
            .expect("an 8-bit keyframe must parse despite the 10-bit request");
        assert_eq!(actual.pixel_format, VideoPixelFormat::Ayuv);
        assert_eq!(actual.transfer_function, VideoTransferFunction::Sdr);
        assert_eq!(
            plan_bitstream_decoder(requested, actual),
            Some(BitstreamDecoder::FullChroma),
            "real 4:4:4 keeps the full-chroma decoder"
        );
    }
    #[cfg(feature = "nvdec-gpu-interop")]
    #[test]
    #[ignore = "requires NVIDIA CUDA/D3D11 interop and low-latency mode"]
    fn gpu_plane_pool_is_bounded_and_leases_outlive_decoder() {
        use ::windows::Win32::Foundation::HMODULE;
        use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use ::windows::Win32::Graphics::Direct3D11::*;
        use ::windows::Win32::Graphics::Dxgi::IDXGIAdapter;
        let mut device = None;
        let mut immediate = None;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate),
            )
            .unwrap();
        }
        let device = device.unwrap();
        let immediate = immediate.unwrap();
        let decoder = NonNull::new(unsafe { on_nvdec_open(1920, 1080, 10) }).unwrap();
        assert_eq!(
            unsafe { on_nvdec_enable_gpu(decoder.as_ptr(), device.as_raw()) },
            0
        );
        let precision = include_bytes!("../../fixtures/probe/hevc-y410-pq-precision.hevc");
        let black = include_bytes!("../../fixtures/probe/hevc-y410-pq.hevc");
        let receive = |n: i64| {
            let sample = if n == 0 {
                precision.as_slice()
            } else {
                black.as_slice()
            };
            assert_eq!(
                unsafe {
                    on_nvdec_send(
                        decoder.as_ptr(),
                        sample.as_ptr(),
                        sample.len() as i32,
                        n * 83333,
                        83333,
                        1,
                    )
                },
                0
            );
            let mut info = FrameInfo::default();
            let mut slot = std::ptr::null_mut();
            let result = unsafe { on_nvdec_receive_gpu(decoder.as_ptr(), &mut info, &mut slot) };
            assert_eq!(info.pts, n * 83333);
            (
                result,
                NonNull::new(slot).map(|slot| unsafe { GpuPlanes::from_slot(slot) }),
            )
        };
        let mut held = Vec::new();
        for n in 0..8 {
            let (result, frame) = receive(n);
            assert_eq!(result, 1);
            held.push(frame.unwrap());
        }
        let identities: std::collections::HashSet<_> = held
            .iter()
            .map(|p| p.textures[0].as_ref().unwrap().as_raw() as usize)
            .collect();
        assert_eq!(
            identities.len(),
            8,
            "leased surfaces must not be overwritten"
        );
        let (result, dropped) = receive(8);
        assert_eq!(result, 2, "a ninth retained frame must not grow the pool");
        assert!(dropped.is_none());
        let reusable = held.last().unwrap().textures[0].as_ref().unwrap().as_raw();
        held.pop();
        unsafe { on_nvdec_reset(decoder.as_ptr()) };
        let (result, replacement) = receive(9);
        assert_eq!(result, 1);
        let replacement = replacement.unwrap();
        assert_eq!(replacement.textures[0].as_ref().unwrap().as_raw(), reusable);
        held.push(replacement);
        unsafe { on_nvdec_close(decoder.as_ptr()) };
        // A retained precision frame survives later black frames, pool pressure,
        // reset and decoder shutdown with its original samples intact.
        let texture = held[0].textures[0].as_ref().unwrap();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap()
        };
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            immediate.CopyResource(&staging, texture);
            immediate
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            assert_eq!(*mapped.pData.cast::<u16>().add(500) >> 6, 564);
            immediate.Unmap(&staging, 0);
        }
        drop(held);
    }
    #[test]
    #[ignore = "requires NVIDIA HEVC hardware and FFmpeg CUVID runtime"]
    fn nvdec_keyframe_reset_discards_stale_frames_and_preserves_layout() {
        for (sample, layout) in [
            (
                include_bytes!("../../fixtures/probe/hevc-y410-precision.hevc").as_slice(),
                1,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-p010-sdr.hevc").as_slice(),
                3,
            ),
        ] {
            let handle = NonNull::new(unsafe { on_nvdec_open(1920, 1080, 10) }).unwrap();
            let mut output = vec![0u32; 1920 * 1080];
            for generation in 0..5i64 {
                let start = std::time::Instant::now();
                unsafe { on_nvdec_reset(handle.as_ptr()) };
                eprintln!(
                    "layout={layout} generation={generation} reset_us={}",
                    start.elapsed().as_micros()
                );
                let base = generation * 10_000_000;
                let mut received = Vec::new();
                for n in 0..3i64 {
                    assert_eq!(
                        unsafe {
                            on_nvdec_send(
                                handle.as_ptr(),
                                sample.as_ptr(),
                                sample.len() as i32,
                                base + n * 166667,
                                166667,
                                1,
                            )
                        },
                        0
                    );
                    loop {
                        let mut info = FrameInfo::default();
                        let result = unsafe {
                            on_nvdec_receive(
                                handle.as_ptr(),
                                output.as_mut_ptr(),
                                output.len(),
                                &mut info,
                            )
                        };
                        assert!(result >= 0, "receive failed: {result}");
                        if result == 0 {
                            break;
                        }
                        assert_eq!(info.output_layout, layout);
                        received.push(info.pts);
                    }
                }
                let expected = if std::env::var("OPENNOW_NVDEC_LOW_LATENCY").as_deref() == Ok("1") {
                    vec![base, base + 166667, base + 333334]
                } else {
                    vec![base, base + 166667]
                };
                assert_eq!(
                    received, expected,
                    "no old parser frame may cross a keyframe reset"
                );
                // Leave a submitted AU undelivered on both routes. The next
                // reset must discard it instead of leaking its old timestamp.
                assert_eq!(
                    unsafe {
                        on_nvdec_send(
                            handle.as_ptr(),
                            sample.as_ptr(),
                            sample.len() as i32,
                            base + 500001,
                            166667,
                            1,
                        )
                    },
                    0
                );
            }
            unsafe { on_nvdec_close(handle.as_ptr()) };
        }
    }
    #[test]
    #[ignore = "requires NVIDIA HEVC 4:4:4 hardware and FFmpeg CUVID runtime"]
    fn nvdec_live_packets_preserve_timestamps_without_draining() {
        let sample = include_bytes!("../../fixtures/probe/hevc-y410-precision.hevc");
        for restart in 0..3 {
            let handle =
                NonNull::new(unsafe { on_nvdec_open(1920, 1080, 10) }).expect("NVDEC open");
            let mut output = vec![0u32; 1920 * 1080];
            let mut received = Vec::new();
            for n in 0..12i64 {
                let pts = 1000000 + n * 166667;
                assert_eq!(
                    unsafe {
                        on_nvdec_send(
                            handle.as_ptr(),
                            sample.as_ptr(),
                            sample.len() as i32,
                            pts,
                            166667,
                            1,
                        )
                    },
                    0
                );
                loop {
                    let mut info = FrameInfo::default();
                    let r = unsafe {
                        on_nvdec_receive(
                            handle.as_ptr(),
                            output.as_mut_ptr(),
                            output.len(),
                            &mut info,
                        )
                    };
                    assert!(r >= 0, "NVDEC receive {r}");
                    if r == 0 {
                        break;
                    }
                    received.push(info.pts);
                }
                if std::env::var("OPENNOW_NVDEC_LOW_LATENCY").as_deref() == Ok("1") {
                    assert_eq!(
                        received.len(),
                        n as usize + 1,
                        "frame must emerge before the next AU"
                    );
                }
            }
            let expected = if std::env::var("OPENNOW_NVDEC_LOW_LATENCY").as_deref() == Ok("1") {
                12
            } else {
                11
            };
            assert_eq!(received.len(), expected, "unexpected parser holdback");
            for (i, pts) in received.iter().enumerate() {
                assert_eq!(*pts, 1000000 + i as i64 * 166667);
            }
            unsafe { on_nvdec_close(handle.as_ptr()) };
            eprintln!(
                "NVDEC live restart={restart}: {} frames retain exact timestamps without drain",
                received.len()
            );
        }
    }
}
