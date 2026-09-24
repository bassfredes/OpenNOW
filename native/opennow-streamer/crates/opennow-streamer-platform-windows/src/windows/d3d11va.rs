//! D3D11VA (DXVA) HEVC 4:4:4 decode route: hardware decode straight into
//! DXGI Y410/AYUV textures on the D3D11 device Qt presents from, with no CPU
//! copy — the path the official client uses ("DX11FrameFilterHandler ...
//! format 0x65" = DXGI_FORMAT_Y410). Capability of the running GPU/driver is
//! probed through `ID3D11VideoDevice::GetVideoDecoderProfile` +
//! `CheckVideoDecoderFormat` before any decoder is created, because the
//! whole route exists only when the driver advertises an HEVC 4:4:4 profile
//! that accepts the matching surface (Y410 for 10-bit, AYUV for 8-bit).

use super::decoder::{DecodedVideoFrame, DecoderDevice};
use super::nvdec::{FrameInfo, decoded_color};
use crate::aperture::VideoAperture;
use crate::{
    EncodedVideoFrame, VideoChromaFormat, VideoCodec, VideoFormat, VideoPixelFormat,
    WindowsDecoderMode,
};
use ::windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, ID3D11VideoDevice};
use ::windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_Y410};
use ::windows::core::{GUID, Interface};
use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

/// HEVC profile GUIDs from the Windows SDK dxva.h (10.0.26100). The D3D11
/// video API reuses the DXVA mode GUIDs as decoder profiles.
pub(crate) const MODE_HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);
pub(crate) const MODE_HEVC_MAIN10_444: GUID =
    GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c);
pub(crate) const MODE_HEVC_MAIN10_EXT: GUID =
    GUID::from_u128(0x9cc55490_e37c_4932_8684_4920f9f6409c);

/// What the running driver advertises for 4:4:4 decode surfaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hevc444Capability {
    /// `ModeHEVC_Main10_444` accepts a Y410 (10-bit 4:4:4) surface.
    pub main10_444_y410: bool,
    /// `ModeHEVC_Main_444` accepts an AYUV (8-bit 4:4:4) surface.
    pub main_444_ayuv: bool,
    pub profiles: u32,
    pub accepting: u32,
}

fn is_known_hevc_profile(profile: &GUID) -> bool {
    *profile == MODE_HEVC_MAIN_444
        || *profile == MODE_HEVC_MAIN10_444
        || *profile == MODE_HEVC_MAIN10_EXT
}

/// True when the device exposes `profile` for decoding into `format`.
/// `ID3D11VideoDevice::CheckVideoDecoderFormat` validates the pair without
/// allocating anything; the decoder creation attempt later proves the size.
pub(crate) fn profile_accepts_format(
    video_device: &ID3D11VideoDevice,
    profile: &GUID,
    format: DXGI_FORMAT,
) -> Result<bool, String> {
    let supported = unsafe {
        video_device
            .CheckVideoDecoderFormat(profile, format)
            .map_err(|error| format!("CheckVideoDecoderFormat({profile:?}): {error}"))?
    };
    Ok(supported.as_bool())
}

/// Probe every decoder profile the device advertises, log the ones accepting
/// a 4:4:4 surface, and report whether the known HEVC 4:4:4 profiles pass —
/// the exact D3D11 capability question the official client answers with
/// `getMaxDecoderCapability_DX11`.
pub(crate) fn probe_hevc_444_support(video_device: &ID3D11VideoDevice) -> Hevc444Capability {
    let mut capability = Hevc444Capability {
        profiles: unsafe { video_device.GetVideoDecoderProfileCount() },
        ..Hevc444Capability::default()
    };
    for index in 0..capability.profiles {
        let Ok(profile) = (unsafe { video_device.GetVideoDecoderProfile(index) }) else {
            continue;
        };
        let y410 =
            profile_accepts_format(video_device, &profile, DXGI_FORMAT_Y410).unwrap_or(false);
        let ayuv =
            profile_accepts_format(video_device, &profile, DXGI_FORMAT_AYUV).unwrap_or(false);
        if !y410 && !ayuv {
            continue;
        }
        capability.accepting += 1;
        video_log!(
            "D3D11VA capability: profile[{index}]={profile:?} y410={y410} ayuv={ayuv} knownHevcExt={}",
            is_known_hevc_profile(&profile)
        );
        if profile == MODE_HEVC_MAIN10_444 && y410 {
            capability.main10_444_y410 = true;
        }
        if profile == MODE_HEVC_MAIN_444 && ayuv {
            capability.main_444_ayuv = true;
        }
    }
    video_log!(
        "D3D11VA HEVC 4:4:4 probe: profiles={} fourFourFourAccepting={} main10_444_y410={} main_444_ayuv={}",
        capability.profiles,
        capability.accepting,
        capability.main10_444_y410,
        capability.main_444_ayuv
    );
    capability
}

unsafe extern "C" {
    fn on_d3d11va_open(
        device: *mut c_void,
        width: i32,
        height: i32,
        depth: i32,
        error: *mut c_char,
        error_len: i32,
    ) -> *mut c_void;
    fn on_d3d11va_send(
        decoder: *mut c_void,
        data: *const u8,
        size: i32,
        pts: i64,
        duration: i64,
        key: i32,
    ) -> i32;
    fn on_d3d11va_drain(decoder: *mut c_void) -> i32;
    fn on_d3d11va_flush(decoder: *mut c_void);
    fn on_d3d11va_close(decoder: *mut c_void);
    fn on_d3d11va_receive(
        decoder: *mut c_void,
        texture: *mut *mut c_void,
        subresource: *mut u32,
        lease: *mut *mut c_void,
        info: *mut FrameInfo,
    ) -> i32;
    fn on_d3d11va_release_lease(lease: *mut c_void);
}

/// Pins one decoded frame's slot in FFmpeg's hardware pool: the presentation
/// path may hold the frame while the decoder keeps cycling, and the pool
/// must not hand the same texture to a later decode until this drops.
pub(super) struct D3d11vaLease(*mut c_void);

unsafe impl Send for D3d11vaLease {}

impl Drop for D3d11vaLease {
    fn drop(&mut self) {
        unsafe { on_d3d11va_release_lease(self.0) };
    }
}

pub(super) struct D3d11vaDecoder {
    handle: NonNull<c_void>,
    format: VideoFormat,
    timestamps: VecDeque<(i64, i64)>,
    stopped: bool,
    credit: bool,
}

impl D3d11vaDecoder {
    pub(super) fn new<G: DecoderDevice>(
        g: &G,
        format: VideoFormat,
        _mode: WindowsDecoderMode,
    ) -> Result<Self, String> {
        format.validate().map_err(|error| error.to_string())?;
        if format.codec != VideoCodec::H265 {
            return Err("D3D11VA route is HEVC-only".to_owned());
        }
        let depth = format.pixel_format.bit_depth();
        // The device is the documented sharing boundary (same dance the MF
        // and NVDEC paths use): decode happens on Qt's multithread-protected
        // device/context pair.
        let manager = g.device_manager();
        let device = unsafe {
            let handle = manager
                .OpenDeviceHandle()
                .map_err(|error| error.to_string())?;
            let mut raw = std::ptr::null_mut();
            let result = manager.LockDevice(handle, &ID3D11Device::IID, &mut raw, true);
            if result.is_ok() {
                let _ = manager.UnlockDevice(handle, false);
            }
            let _ = manager.CloseDeviceHandle(handle);
            result.map_err(|error| format!("D3D11VA bridge get D3D11 device: {error}"))?;
            if raw.is_null() {
                return Err("D3D11VA bridge received null D3D11 device".into());
            }
            ID3D11Device::from_raw(raw)
        };
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| format!("ID3D11VideoDevice cast: {error}"))?;
        let capability = probe_hevc_444_support(&video_device);
        let supported = match depth {
            10 => capability.main10_444_y410,
            _ => capability.main_444_ayuv,
        };
        if !supported {
            return Err(format!(
                "driver does not expose HEVC 4:4:4 DXVA for {depth}-bit 4:4:4 surfaces (profiles={} accepting={})",
                capability.profiles, capability.accepting
            ));
        }
        let mut error = [0_i8; 256];
        let handle = NonNull::new(unsafe {
            on_d3d11va_open(
                device.as_raw().cast(),
                format.width as i32,
                format.height as i32,
                depth as i32,
                error.as_mut_ptr(),
                error.len() as i32,
            )
        })
        .ok_or_else(|| {
            let detail = std::ffi::CStr::from_bytes_until_nul(bytemuck_or_trim(&error))
                .map(|text| text.to_string_lossy().into_owned())
                .unwrap_or_default();
            if detail.is_empty() {
                "Could not initialize the D3D11VA HEVC decoder".to_owned()
            } else {
                format!("Could not initialize the D3D11VA HEVC decoder: {detail}")
            }
        })?;
        video_log!(
            "D3D11VA HEVC 4:4:4 route enabled: {}x{} depth={} zero-copy={} deviceShared=true",
            format.width,
            format.height,
            depth,
            if depth == 10 { "Y410" } else { "AYUV" }
        );
        Ok(Self {
            handle,
            format,
            timestamps: VecDeque::with_capacity(16),
            stopped: false,
            credit: true,
        })
    }

    pub(super) fn wants_input(&self) -> bool {
        !self.stopped && self.credit && self.timestamps.len() < 16
    }

    pub(super) fn format(&self) -> VideoFormat {
        self.format
    }

    /// Flush references in place for a recovery keyframe: the device and
    /// decoder survive, so recovery never becomes a restart storm.
    pub(super) fn reset_at_keyframe(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        unsafe { on_d3d11va_flush(self.handle.as_ptr()) };
        self.timestamps.clear();
        self.credit = true;
        video_log!("D3D11VA keyframe reset flushed references in place elapsed_us=0");
        true
    }

    pub(super) fn submit(&mut self, frame: EncodedVideoFrame) -> Result<(), String> {
        if self.stopped
            || !self.credit
            || self.timestamps.len() >= 16
            || frame.codec != VideoCodec::H265
            || frame.data.is_empty()
            || frame.data.len() > 32 * 1024 * 1024
        {
            return Err("D3D11VA bridge rejected input state/size/codec".into());
        }
        let result = unsafe {
            on_d3d11va_send(
                self.handle.as_ptr(),
                frame.data.as_ptr(),
                frame.data.len() as i32,
                frame.timestamp_100ns,
                frame.duration_100ns,
                i32::from(frame.key_frame),
            )
        };
        if result < 0 {
            return Err(format!("D3D11VA send failed: {result}"));
        }
        self.timestamps
            .push_back((frame.timestamp_100ns, frame.duration_100ns));
        self.credit = false;
        Ok(())
    }

    pub(super) fn poll(
        &mut self,
        frames: &mut VecDeque<DecodedVideoFrame>,
    ) -> Result<usize, String> {
        if self.stopped {
            return Ok(0);
        }
        let mut texture: *mut c_void = std::ptr::null_mut();
        let mut subresource = 0_u32;
        let mut lease: *mut c_void = std::ptr::null_mut();
        let mut info = FrameInfo::default();
        let result = unsafe {
            on_d3d11va_receive(
                self.handle.as_ptr(),
                &mut texture,
                &mut subresource,
                &mut lease,
                &mut info,
            )
        };
        if result < 0 {
            return Err(match result {
                -4 => "D3D11VA returned a software frame; hwaccel disengaged".to_owned(),
                -5 => format!(
                    "D3D11VA pool surface mismatch: expected {:?} at {}x{}",
                    if self.format.pixel_format.bit_depth() == 10 {
                        DXGI_FORMAT_Y410
                    } else {
                        DXGI_FORMAT_AYUV
                    },
                    self.format.width,
                    self.format.height
                ),
                _ => format!("D3D11VA receive failed: {result}"),
            });
        }
        self.credit = true;
        if result == 0 {
            return Ok(0);
        }
        let position = self
            .timestamps
            .iter()
            .position(|(pts, _)| *pts == info.pts)
            .ok_or("D3D11VA output timestamp does not match submitted input")?;
        let (_, duration) = self.timestamps.remove(position).unwrap();
        if frames.len() >= crate::ADAPTIVE_VIDEO_QUEUE_CAPACITY {
            // The presentation consumer is behind: release this frame's lease
            // and texture instead of failing — a backlog must never poison
            // the decoder or trigger a restart.
            drop(D3d11vaLease(lease));
            unsafe { drop(ID3D11Texture2D::from_raw(texture.cast())) };
            return Ok(0);
        }
        let updated = decoded_color(self.format, &info)?;
        if updated != self.format {
            video_log!(
                "D3D11VA decoded color transition: {:?} -> {:?}",
                self.format.transfer_function,
                updated.transfer_function
            );
            self.format = updated;
        }
        let pixel_format = if info.output_layout == 1 {
            VideoPixelFormat::Y410
        } else {
            VideoPixelFormat::Ayuv
        };
        if self.format.pixel_format != pixel_format {
            self.format.pixel_format = pixel_format;
            self.format.chroma_format = VideoChromaFormat::Cs444;
        }
        self.format.validate().map_err(|error| error.to_string())?;
        let decoded = unsafe {
            ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw(texture.cast())
        };
        frames.push_back(DecodedVideoFrame {
            format: self.format,
            aperture: VideoAperture::new(self.format.width, self.format.height, None)?,
            texture: decoded,
            subresource,
            timestamp_100ns: info.pts,
            duration_100ns: duration,
            _sample: None,
            #[cfg(feature = "nvdec-gpu-interop")]
            gpu_planes: None,
            _lease: Some(D3d11vaLease(lease)),
        });
        Ok(1)
    }

    /// Signal end of stream so buffered reordered frames are released.
    pub(super) fn drain(&self) -> i32 {
        unsafe { on_d3d11va_drain(self.handle.as_ptr()) }
    }

    pub(super) fn probe_frame(&mut self, data: &[u8]) -> Result<DecodedVideoFrame, String> {
        let mut produced = VecDeque::new();
        let keyframe = EncodedVideoFrame {
            codec: VideoCodec::H265,
            data: data.to_vec(),
            timestamp_100ns: 0,
            duration_100ns: self.format.frame_duration_100ns(),
            key_frame: true,
            reset_decoder: false,
        };
        self.submit(keyframe)?;
        let _ = self.drain();
        for _ in 0..8 {
            self.poll(&mut produced)?;
            if let Some(frame) = produced.pop_front() {
                return Ok(frame);
            }
        }
        Err("D3D11VA probe returned no frame".into())
    }

    pub(super) fn stop(&mut self) {
        if !self.stopped {
            unsafe { on_d3d11va_close(self.handle.as_ptr()) };
            self.stopped = true;
            self.timestamps.clear();
        }
    }
}

impl Drop for D3d11vaDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `error` is an `i8` buffer filled by C `snprintf`; trim at the first NUL.
fn bytemuck_or_trim(buffer: &[i8]) -> &[u8] {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    // SAFETY: every byte before the first NUL is a valid ASCII byte from
    // snprintf; reinterpretation of i8<128 to u8 is sound, negative bytes
    // are past the NUL and excluded.
    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), end) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::Win32::Foundation::HMODULE;
    use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use ::windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    };

    fn test_device() -> ID3D11Device {
        let mut device: Option<ID3D11Device> = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None::<&::windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("D3D11 hardware device");
        }
        device.expect("D3D11 device")
    }

    /// Capability discovery for the running GPU (the exact check the official
    /// client performs with `getMaxDecoderCapability_DX11`). Fails when the
    /// driver does not expose HEVC Main10 4:4:4 for Y410, because the
    /// D3D11VA 4:4:4 route cannot run without it; the bitstream selection
    /// then falls back to NVDEC.
    #[test]
    fn d3d11va_reports_hevc_444_profiles_on_this_gpu() {
        let video_device: ID3D11VideoDevice = test_device()
            .cast()
            .expect("D3D11 device exposes ID3D11VideoDevice");
        let capability = probe_hevc_444_support(&video_device);
        assert!(
            capability.main10_444_y410,
            "this GPU/driver does not expose HEVC Main10 4:4:4 DXVA for DXGI_FORMAT_Y410; the D3D11VA 4:4:4 route cannot run here"
        );
        assert!(
            capability.main_444_ayuv,
            "this GPU/driver does not expose HEVC Main 4:4:4 DXVA for DXGI_FORMAT_AYUV"
        );
    }
}
