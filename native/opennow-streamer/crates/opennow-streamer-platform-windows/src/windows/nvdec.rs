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
use std::{collections::VecDeque, ffi::c_void, ptr::NonNull};

#[repr(C)]
#[derive(Default)]
struct FrameInfo {
    pts: i64,
    duration: i64,
    range: i32,
    primaries: i32,
    transfer: i32,
    matrix: i32,
    pixel_format: i32,
    width: i32,
    height: i32,
    source_depth: i32,
    output_layout: i32,
}
unsafe extern "C" {
    fn on_hevc_keyframe_layout(
        data: *const u8,
        size: i32,
        width: i32,
        height: i32,
        depth: i32,
    ) -> i32;
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
        }
    }
    pub(super) fn format(&self) -> VideoFormat {
        match self {
            Self::Mf(d) => d.format(),
            Self::Nv(d) => d.format,
        }
    }
    pub(super) fn stop(&mut self) {
        match self {
            Self::Mf(d) => d.stop(),
            Self::Nv(d) => d.stop(),
        }
    }
    pub(super) fn reset_at_keyframe(&mut self) -> bool {
        let Self::Nv(d) = self else { return false };
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
    pub(super) fn submit(&mut self, frame: EncodedVideoFrame) -> Result<(), String> {
        match self {
            Self::Mf(d) => d.submit(frame),
            Self::Nv(d) => d.submit(frame),
        }
    }
    // The request remains 4:4:4. Choose the decoder only from actual bitstream
    // metadata, before submitting this unmodified random-access unit.
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
        let native_420 = actual.chroma_format == VideoChromaFormat::Cs420;
        if native_420 == matches!(self, Self::Mf(_)) {
            return Ok(false);
        }
        let replacement = if native_420 {
            Self::Mf(MfDecoder::new(device, actual, mode)?)
        } else {
            Self::Nv(NvDecoder::new(device, actual)?)
        };
        video_log!(
            "HEVC bitstream decoder selection: requested={:?} actual={:?} depth={} route={}; request and compressed pixels unchanged",
            requested.chroma_format,
            actual.chroma_format,
            actual.pixel_format.bit_depth(),
            if native_420 {
                "Media Foundation D3D11"
            } else {
                "NVDEC full-chroma"
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
        }
    }
    pub(super) fn probe_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        match self {
            Self::Mf(d) => d.probe_frame(data),
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

fn keyframe_format(requested: VideoFormat, frame: &EncodedVideoFrame) -> Option<VideoFormat> {
    if requested.codec != VideoCodec::H265
        || frame.codec != VideoCodec::H265
        || requested.chroma_format != VideoChromaFormat::Cs444
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
            requested.pixel_format.bit_depth() as i32,
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

fn decoded_color(mut format: VideoFormat, info: &FrameInfo) -> Result<VideoFormat, String> {
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
    #[test]
    fn hevc_keyframe_metadata_distinguishes_native_chroma_and_rejects_mismatch() {
        let samples: &[(&[u8], i32, i32, i32, i32)] = &[
            (
                include_bytes!("../../fixtures/probe/hevc-p010-pq.hevc"),
                1920,
                1080,
                10,
                3,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-p010-5k-pq.hevc"),
                5120,
                2880,
                10,
                3,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-y410-pq-precision.hevc"),
                1920,
                1080,
                10,
                1,
            ),
            (
                include_bytes!("../../fixtures/probe/hevc-ayuv-sdr.hevc"),
                1920,
                1080,
                8,
                0,
            ),
        ];
        for &(data, width, height, depth, layout) in samples {
            let inspect = |bytes: &[u8], w, d| unsafe {
                on_hevc_keyframe_layout(bytes.as_ptr(), bytes.len() as i32, w, height, d)
            };
            assert_eq!(inspect(data, width, depth), layout);
            assert_eq!(
                inspect(data, width + 2, depth),
                -1,
                "do not reroute a different resolution"
            );
            assert_eq!(
                inspect(data, width, if depth == 10 { 8 } else { 10 }),
                -1,
                "do not downgrade or fabricate bit depth"
            );
            assert_eq!(
                inspect(&data[..8], width, depth),
                -1,
                "incomplete headers prove no layout"
            );
            assert_eq!(inspect(&[], width, depth), -1);
        }
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
