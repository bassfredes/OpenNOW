/* Bounded CUDA -> D3D11 plane copies. No CPU pixel transfer or chroma reduction.
 * Slots are leased until rendering submits its commands. CUDA map/unmap orders
 * reuse against earlier D3D11 reads. Each slot retains the CUDA device context,
 * so a displayed frame may safely outlive decoder shutdown. */
struct ONGpuSlot {
    volatile LONG refs;
    AVBufferRef *device_ref;
    CUcontext context;
    CUgraphicsResource resources[3];
    ID3D11Texture2D *textures[3];
    int layout, planes, width, height;
};

void on_nvdec_gpu_release(ONGpuSlot *slot) {
    if (!slot || InterlockedDecrement(&slot->refs) != 0) return;
    CUcontext popped;
    int pushed = cuCtxPushCurrent(slot->context) == CUDA_SUCCESS;
    for (int p = 0; p < 3; ++p) {
        if (pushed && slot->resources[p]) cuGraphicsUnregisterResource(slot->resources[p]);
        if (slot->textures[p]) ID3D11Texture2D_Release(slot->textures[p]);
    }
    if (pushed) cuCtxPopCurrent(&popped);
    av_buffer_unref(&slot->device_ref);
    free(slot);
}

int on_nvdec_enable_gpu(ONNvdec *d, ID3D11Device *device) {
    if (!d || !device || !d->low_latency || d->depth != 10 || d->d3d_device) return -1;
    d->d3d_device = device;
    ID3D11Device_AddRef(device);
    return 0;
}

static ONGpuSlot *on_gpu_slot_create(ONNvdec *d, int layout) {
    ONGpuSlot *slot = calloc(1, sizeof(*slot));
    if (!slot) return NULL;
    slot->refs = 1; slot->layout = layout; slot->planes = layout == 1 ? 3 : 2;
    slot->width = d->width; slot->height = d->height;
    slot->device_ref = av_buffer_ref(d->ctx->hw_device_ctx);
    if (!slot->device_ref) { free(slot); return NULL; }
    AVHWDeviceContext *hw = (AVHWDeviceContext *)slot->device_ref->data;
    slot->context = ((AVCUDADeviceContext *)hw->hwctx)->cuda_ctx;
    CUcontext popped;
    if (cuCtxPushCurrent(slot->context) != CUDA_SUCCESS) {
        av_buffer_unref(&slot->device_ref); free(slot); return NULL;
    }
    int ok = 1;
    for (int p = 0; p < slot->planes && ok; ++p) {
        int uv = layout == 3 && p == 1;
        D3D11_TEXTURE2D_DESC desc = {0};
        desc.Width = d->width / (uv ? 2 : 1); desc.Height = d->height / (uv ? 2 : 1);
        desc.MipLevels = desc.ArraySize = 1;
        desc.Format = uv ? DXGI_FORMAT_R16G16_UINT : DXGI_FORMAT_R16_UINT;
        desc.SampleDesc.Count = 1; desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        ok = SUCCEEDED(ID3D11Device_CreateTexture2D(d->d3d_device, &desc, NULL, &slot->textures[p]));
        if (ok) ok = cuGraphicsD3D11RegisterResource(&slot->resources[p],
            (ID3D11Resource *)slot->textures[p], CU_GRAPHICS_REGISTER_FLAGS_NONE) == CUDA_SUCCESS;
    }
    cuCtxPopCurrent(&popped);
    if (!ok) { on_nvdec_gpu_release(slot); return NULL; }
    return slot;
}

void *on_nvdec_gpu_texture(ONGpuSlot *slot, int plane) {
    return slot && plane >= 0 && plane < slot->planes ? slot->textures[plane] : NULL;
}

/* 1 = leased frame; 0 = needs input; 2 = consumed frame dropped because all
 * eight slots remain leased. Never overwrite a displayed frame or grow a pool. */
int on_nvdec_receive_gpu(ONNvdec *d, ONFrameInfo *info, ONGpuSlot **output) {
    if (!d || !d->d3d_device || !info || !output) return -1;
    *output = NULL;
    av_frame_unref(d->hardware_frame);
    int result = avcodec_receive_frame(d->ctx, d->hardware_frame);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 0;
    if (result < 0) return result;
    AVFrame *f = d->hardware_frame;
    if (f->format != AV_PIX_FMT_CUDA || !f->hw_frames_ctx) return -4;
    AVHWFramesContext *frames = (AVHWFramesContext *)f->hw_frames_ctx->data;
    int layout = frames->sw_format == AV_PIX_FMT_YUV444P16LE ? 1 :
                 frames->sw_format == AV_PIX_FMT_P010LE ? 3 : -1;
    info->pixel_format = frames->sw_format; info->source_depth = 10;
    info->width = f->width; info->height = f->height;
    info->pts = f->pts; info->duration = f->duration;
    info->range = f->color_range; info->primaries = f->color_primaries;
    info->transfer = f->color_trc; info->matrix = f->colorspace;
    info->output_layout = layout;
    if (layout < 0 || (d->ctx->bits_per_raw_sample && d->ctx->bits_per_raw_sample != 10) ||
        f->width != d->width || f->height != d->height ||
        f->decode_error_flags || f->crop_left || f->crop_right || f->crop_top || f->crop_bottom ||
        f->pts == AV_NOPTS_VALUE || (layout == 3 && ((d->width | d->height) & 1))) return -2;
    ONGpuSlot *slot = NULL;
    for (int i = 0; i < ON_GPU_SLOTS; ++i) {
        ONGpuSlot *candidate = d->gpu_slots[i];
        if (candidate && InterlockedCompareExchange(&candidate->refs, 1, 1) != 1) continue;
        if (candidate && candidate->layout != layout) {
            on_nvdec_gpu_release(candidate); d->gpu_slots[i] = candidate = NULL;
        }
        if (!candidate) d->gpu_slots[i] = candidate = on_gpu_slot_create(d, layout);
        if (!candidate) return -5;
        slot = candidate; break;
    }
    if (!slot) return 2;
    CUcontext popped;
    if (cuCtxPushCurrent(slot->context) != CUDA_SUCCESS) return -6;
    CUresult error = cuGraphicsMapResources(slot->planes, slot->resources, 0);
    if (error == CUDA_SUCCESS) {
        for (int p = 0; p < slot->planes && error == CUDA_SUCCESS; ++p) {
            int uv = layout == 3 && p == 1;
            size_t width_bytes = (size_t)d->width * 2;
            size_t rows = d->height / (uv ? 2 : 1);
            if (!f->data[p] || f->linesize[p] < (int)width_bytes) { error = CUDA_ERROR_INVALID_VALUE; break; }
            CUarray array = NULL;
            error = cuGraphicsSubResourceGetMappedArray(&array, slot->resources[p], 0, 0);
            if (error != CUDA_SUCCESS) break;
            CUDA_MEMCPY2D copy = {0};
            copy.srcMemoryType = CU_MEMORYTYPE_DEVICE; copy.srcDevice = (CUdeviceptr)f->data[p];
            copy.srcPitch = f->linesize[p]; copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
            copy.dstArray = array; copy.WidthInBytes = width_bytes; copy.Height = rows;
            error = cuMemcpy2D(&copy);
        }
        CUresult unmap = cuGraphicsUnmapResources(slot->planes, slot->resources, 0);
        if (error == CUDA_SUCCESS) error = unmap;
    }
    cuCtxPopCurrent(&popped);
    if (error != CUDA_SUCCESS) return -7;
    InterlockedIncrement(&slot->refs);
    *output = slot;
    return 1;
}
