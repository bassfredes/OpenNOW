use super::decoder::DecodedVideoFrame;
use super::embedded::MAX_FRAME_SLOTS;
use crate::VideoFormat;
use crate::y410_color::Y410Constants;
use ::windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob,
};
use ::windows::Win32::Graphics::Direct3D11::*;
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R10G10B10A2_UINT, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_Y410, DXGI_SAMPLE_DESC,
};
use ::windows::core::PCSTR;
struct OutputSlot {
    texture: ID3D11Texture2D,
    view: ID3D11RenderTargetView,
}
pub(super) struct Y410Converter {
    device: ID3D11Device,
    immediate: ID3D11DeviceContext,
    deferred: ID3D11DeviceContext,
    input: ID3D11Texture2D,
    input_view: ID3D11ShaderResourceView,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    #[cfg(feature = "nvdec-gpu-interop")]
    planes_pixel: ID3D11PixelShader,
    #[cfg(feature = "nvdec-gpu-interop")]
    plane_layout: ID3D11Buffer,
    quantization: ID3D11Buffer,
    rasterizer: ID3D11RasterizerState,
    slots: [Option<OutputSlot>; MAX_FRAME_SLOTS],
    format: VideoFormat,
    gpu: GpuTimer,
}
impl Y410Converter {
    pub(super) fn new(
        device: &ID3D11Device,
        immediate: &ID3D11DeviceContext,
        format: VideoFormat,
    ) -> Result<Self, String> {
        if !matches!(
            format.pixel_format,
            crate::VideoPixelFormat::Y410 | crate::VideoPixelFormat::P010
        ) {
            return Err("GPU plane conversion requires a ten-bit YUV format".into());
        }
        // Both GPU planar layouts use the same ten-bit code-value conversion.
        let constants = Y410Constants::new(VideoFormat {
            pixel_format: crate::VideoPixelFormat::Y410,
            ..format
        })?;
        let mut input = None;
        let mut input_view = None;
        let mut deferred = None;
        unsafe {
            device
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: format.width,
                        Height: format.height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_Y410,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut input),
                )
                .map_err(|error| format!("create Y410 shader input: {error}"))?;
        }
        let input = input.ok_or("no Y410 shader input")?;
        unsafe {
            device
                .CreateShaderResourceView(
                    &input,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R10G10B10A2_UINT,
                        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                            },
                        },
                    }),
                    Some(&mut input_view),
                )
                .map_err(|error| format!("create exact Y410 UINT view: {error}"))?;
            device
                .CreateDeferredContext(0, Some(&mut deferred))
                .map_err(|error| format!("create Y410 deferred context: {error}"))?;
        }
        let vertex_code = compile(c"vertex_main", c"vs_5_0")?;
        let pixel_code = compile(c"pixel_main", c"ps_5_0")?;
        let mut vertex = None;
        let mut pixel = None;
        unsafe {
            device
                .CreateVertexShader(
                    std::slice::from_raw_parts(
                        vertex_code.GetBufferPointer().cast(),
                        vertex_code.GetBufferSize(),
                    ),
                    None,
                    Some(&mut vertex),
                )
                .map_err(|error| format!("create Y410 vertex shader: {error}"))?;
            device
                .CreatePixelShader(
                    std::slice::from_raw_parts(
                        pixel_code.GetBufferPointer().cast(),
                        pixel_code.GetBufferSize(),
                    ),
                    None,
                    Some(&mut pixel),
                )
                .map_err(|error| format!("create Y410 pixel shader: {error}"))?;
        }
        let mut quantization = None;
        #[cfg(feature = "nvdec-gpu-interop")]
        let (planes_pixel, plane_layout) = {
            let code = compile(c"pixel_planes", c"ps_5_0")?;
            let mut pixel = None;
            let mut layout = None;
            let values: [f32; 4] = [
                if format.pixel_format == crate::VideoPixelFormat::P010 {
                    1.0
                } else {
                    0.0
                },
                0.0,
                if format.chroma_siting == crate::VideoChromaSiting::Left {
                    0.5
                } else {
                    0.0
                },
                0.0,
            ];
            unsafe {
                device
                    .CreatePixelShader(
                        std::slice::from_raw_parts(
                            code.GetBufferPointer().cast(),
                            code.GetBufferSize(),
                        ),
                        None,
                        Some(&mut pixel),
                    )
                    .map_err(|error| format!("create GPU planes shader: {error}"))?;
                device
                    .CreateBuffer(
                        &D3D11_BUFFER_DESC {
                            ByteWidth: 16,
                            Usage: D3D11_USAGE_IMMUTABLE,
                            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                            ..Default::default()
                        },
                        Some(&D3D11_SUBRESOURCE_DATA {
                            pSysMem: values.as_ptr().cast(),
                            ..Default::default()
                        }),
                        Some(&mut layout),
                    )
                    .map_err(|error| format!("create GPU plane layout: {error}"))?;
            }
            (
                pixel.ok_or("no GPU planes shader")?,
                layout.ok_or("no GPU plane layout")?,
            )
        };
        let mut rasterizer = None;
        unsafe {
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: std::mem::size_of::<Y410Constants>() as u32,
                        Usage: D3D11_USAGE_IMMUTABLE,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    Some(&D3D11_SUBRESOURCE_DATA {
                        pSysMem: std::ptr::from_ref(&constants).cast(),
                        ..Default::default()
                    }),
                    Some(&mut quantization),
                )
                .map_err(|error| format!("create Y410 quantization constants: {error}"))?;
            device
                .CreateRasterizerState(
                    &D3D11_RASTERIZER_DESC {
                        FillMode: D3D11_FILL_SOLID,
                        CullMode: D3D11_CULL_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    Some(&mut rasterizer),
                )
                .map_err(|error| format!("create Y410 rasterizer: {error}"))?;
        }
        Ok(Self {
            device: device.clone(),
            immediate: immediate.clone(),
            deferred: deferred.ok_or("no Y410 deferred context")?,
            input,
            input_view: input_view.ok_or("no Y410 shader view")?,
            vertex: vertex.ok_or("no Y410 vertex shader")?,
            pixel: pixel.ok_or("no Y410 pixel shader")?,
            #[cfg(feature = "nvdec-gpu-interop")]
            planes_pixel,
            #[cfg(feature = "nvdec-gpu-interop")]
            plane_layout,
            quantization: quantization.ok_or("no Y410 quantization buffer")?,
            rasterizer: rasterizer.ok_or("no Y410 rasterizer")?,
            slots: std::array::from_fn(|_| None),
            format,
            gpu: GpuTimer::new(device),
        })
    }
    pub(super) fn record(
        &mut self,
        slot: usize,
        frame: &DecodedVideoFrame,
    ) -> Result<&ID3D11Texture2D, String> {
        if slot >= MAX_FRAME_SLOTS || frame.format != self.format {
            return Err("Y410 converter frame or slot mismatch".to_owned());
        }
        let record_started = std::time::Instant::now();
        // Serialize with the D3D11VA decoder's device callbacks (the shared
        // immediate-context lock): the render-thread conversion must never
        // interleave its copy/draw/execute with worker-thread decode
        // submissions — the live-only block-smear hypothesis. Offline tests
        // run single-threaded and cannot see that race.
        #[cfg(feature = "nvdec-experiment")]
        let _shared_context_lock = {
            let started = std::time::Instant::now();
            let guard = super::d3d11va::SharedContextLock::acquire();
            super::render_timing::record_lock_wait_us(super::render_timing::micros_since(started));
            guard
        };
        if self.slots[slot].is_none() {
            let mut texture = None;
            let mut view = None;
            unsafe {
                self.device
                    .CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: self.format.width,
                            Height: self.format.height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0
                                as u32,
                            ..Default::default()
                        },
                        None,
                        Some(&mut texture),
                    )
                    .map_err(|error| format!("create Y410 RGB10A2 slot: {error}"))?;
            }
            let texture = texture.ok_or("no Y410 RGB10A2 slot")?;
            unsafe {
                self.device
                    .CreateRenderTargetView(&texture, None, Some(&mut view))
            }
            .map_err(|error| format!("create Y410 RGB10A2 view: {error}"))?;
            self.slots[slot] = Some(OutputSlot {
                texture,
                view: view.ok_or("no Y410 RGB10A2 view")?,
            });
        }
        let output = self.slots[slot].as_ref().ok_or("no Y410 output slot")?;
        #[cfg(feature = "nvdec-gpu-interop")]
        let plane_views = if let Some(planes) = &frame.gpu_planes {
            let mut views = Vec::with_capacity(3);
            for texture in &planes.textures {
                let mut view = None;
                if let Some(texture) = texture {
                    unsafe {
                        self.device
                            .CreateShaderResourceView(texture, None, Some(&mut view))
                    }
                    .map_err(|error| format!("create GPU plane view: {error}"))?;
                }
                views.push(view);
            }
            Some(views)
        } else {
            None
        };
        let region = D3D11_BOX {
            left: frame.aperture.x,
            top: frame.aperture.y,
            front: 0,
            right: frame.aperture.x + frame.aperture.width,
            bottom: frame.aperture.y + frame.aperture.height,
            back: 1,
        };
        unsafe {
            #[cfg(feature = "nvdec-gpu-interop")]
            let planar = plane_views.is_some();
            #[cfg(not(feature = "nvdec-gpu-interop"))]
            let planar = false;
            if !planar {
                self.deferred.CopySubresourceRegion(
                    &self.input,
                    0,
                    0,
                    0,
                    0,
                    &frame.texture,
                    frame.subresource,
                    Some(&region),
                );
            }
            self.deferred
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.deferred.VSSetShader(&self.vertex, None);
            self.deferred.PSSetShader(&self.pixel, None);
            self.deferred
                .PSSetShaderResources(0, Some(&[Some(self.input_view.clone())]));
            self.deferred
                .PSSetConstantBuffers(0, Some(&[Some(self.quantization.clone())]));
            #[cfg(feature = "nvdec-gpu-interop")]
            if let Some(views) = &plane_views {
                self.deferred.PSSetShader(&self.planes_pixel, None);
                self.deferred.PSSetShaderResources(1, Some(views));
                self.deferred
                    .PSSetConstantBuffers(1, Some(&[Some(self.plane_layout.clone())]));
            }
            self.deferred.RSSetState(&self.rasterizer);
            self.deferred.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: self.format.width as f32,
                Height: self.format.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            self.deferred
                .OMSetRenderTargets(Some(&[Some(output.view.clone())]), None);
            self.deferred.Draw(3, 0);
            let mut commands = None;
            self.deferred
                .FinishCommandList(false, Some(&mut commands))
                .map_err(|error| format!("finish Y410 conversion commands: {error}"))?;
            let commands = commands.ok_or("no Y410 conversion commands")?;
            // Bracket the conversion's GPU execution with timestamp queries;
            // the ring is read back several frames later with the non-blocking
            // flag, so no path ever waits on the GPU for diagnostics.
            self.gpu.begin(&self.immediate);
            let started = std::time::Instant::now();
            self.immediate.ExecuteCommandList(&commands, true);
            super::render_timing::record_execute_us(super::render_timing::micros_since(started));
            self.gpu.end(&self.immediate);
            self.device
                .GetDeviceRemovedReason()
                .map_err(|error| format!("Y410 conversion device lost: {error}"))?;
        }
        super::render_timing::record_record_us(super::render_timing::micros_since(record_started));
        Ok(&output.texture)
    }
}
fn compile(entry: &std::ffi::CStr, target: &std::ffi::CStr) -> Result<ID3DBlob, String> {
    let source = include_bytes!("y410.hlsl");
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors
            .map(|errors| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast(),
                    errors.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("compile Y410 shader: {error}: {detail}"));
    }
    code.ok_or_else(|| "no Y410 shader bytecode".to_owned())
}

/// Ring of D3D11 timestamp queries measuring the conversion's GPU time on the
/// shared immediate context: `Begin(disjoint) -> End(t0) -> conversion ->
/// End(t1) -> End(disjoint)`, then read back `GPU_TIMER_RING` frames later
/// with `D3D11_ASYNC_GETDATA_DONOTWAIT`. A result that is not ready leaves the
/// sentinel buffers untouched and the sample is skipped — no path ever waits
/// on the GPU for diagnostics. Query-creation failure leaves the timer inert;
/// instrumentation must never break presentation.
const GPU_TIMER_RING: usize = 8;

struct GpuTimer {
    disjoint: [Option<ID3D11Query>; GPU_TIMER_RING],
    start: [Option<ID3D11Query>; GPU_TIMER_RING],
    end: [Option<ID3D11Query>; GPU_TIMER_RING],
    armed: [bool; GPU_TIMER_RING],
    cursor: usize,
}

impl GpuTimer {
    fn new(device: &ID3D11Device) -> Self {
        let create = |query_type: D3D11_QUERY| -> Option<ID3D11Query> {
            let mut query: Option<ID3D11Query> = None;
            let description = D3D11_QUERY_DESC {
                Query: query_type,
                MiscFlags: 0,
            };
            unsafe { device.CreateQuery(&description, Some(&mut query)) }.ok()?;
            query
        };
        Self {
            disjoint: std::array::from_fn(|_| create(D3D11_QUERY_TIMESTAMP_DISJOINT)),
            start: std::array::from_fn(|_| create(D3D11_QUERY_TIMESTAMP)),
            end: std::array::from_fn(|_| create(D3D11_QUERY_TIMESTAMP)),
            armed: [false; GPU_TIMER_RING],
            cursor: 0,
        }
    }

    fn slot_queries(&self, slot: usize) -> Option<(&ID3D11Query, &ID3D11Query, &ID3D11Query)> {
        Some((
            self.disjoint.get(slot)?.as_ref()?,
            self.start.get(slot)?.as_ref()?,
            self.end.get(slot)?.as_ref()?,
        ))
    }

    /// Resolves the slot's previous cycle (issued eight frames ago) without
    /// blocking, then opens the next timing scope.
    fn begin(&mut self, context: &ID3D11DeviceContext) {
        let slot = self.cursor;
        self.try_collect(context, slot);
        let Some((disjoint, start, _)) = self.slot_queries(slot) else {
            return;
        };
        unsafe {
            context.Begin(disjoint);
            context.End(start);
        }
        self.armed[slot] = true;
    }

    fn end(&mut self, context: &ID3D11DeviceContext) {
        let slot = self.cursor;
        if !self.armed[slot] {
            return;
        }
        self.armed[slot] = false;
        if let Some((disjoint, _, end)) = self.slot_queries(slot) {
            unsafe {
                context.End(end);
                context.End(disjoint);
            }
        }
        self.cursor = (slot + 1) % GPU_TIMER_RING;
    }

    fn try_collect(&self, context: &ID3D11DeviceContext, slot: usize) {
        if self.armed[slot] {
            // The previous cycle for this slot never closed; re-arm instead
            // of reading half-written data.
            return;
        }
        let Some((disjoint, start, end)) = self.slot_queries(slot) else {
            return;
        };
        // `GetData` reports "not ready" as S_FALSE, which the binding maps to
        // success with the buffers left untouched — the sentinels decide.
        // D3D11 GetData is non-blocking by default; DONOTFLUSH keeps it from
        // forcing pipeline completion, so nothing ever waits on the GPU here.
        let mut disjoint_data = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT {
            Frequency: 0,
            Disjoint: true.into(),
        };
        let mut start_data = u64::MAX;
        let mut end_data = u64::MAX;
        let flags = D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32;
        unsafe {
            if context
                .GetData(
                    disjoint,
                    Some(std::ptr::from_mut(&mut disjoint_data).cast()),
                    u32::try_from(std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>())
                        .unwrap_or(0),
                    flags,
                )
                .is_err()
            {
                return;
            }
            if context
                .GetData(
                    start,
                    Some(std::ptr::from_mut(&mut start_data).cast()),
                    u32::try_from(std::mem::size_of::<u64>()).unwrap_or(0),
                    flags,
                )
                .is_err()
            {
                return;
            }
            if context
                .GetData(
                    end,
                    Some(std::ptr::from_mut(&mut end_data).cast()),
                    u32::try_from(std::mem::size_of::<u64>()).unwrap_or(0),
                    flags,
                )
                .is_err()
            {
                return;
            }
        }
        if disjoint_data.Frequency == 0
            || disjoint_data.Disjoint.as_bool()
            || start_data == u64::MAX
            || end_data == u64::MAX
            || end_data < start_data
        {
            return;
        }
        let micros = (end_data - start_data) as u128 * 1_000_000 / disjoint_data.Frequency as u128;
        super::render_timing::record_gpu_convert_us(micros.min(u64::MAX as u128) as u64);
    }
}
