#include "streaming/rendering/NativeStreamRenderCallback.h"
#include "streaming/rendering/HdrOutput.h"

#include "streaming/NativeStreamRuntime.h"
#include "streaming/rendering/LinuxVulkanGraphics.h"
#include "streaming/rendering/StreamPresentTimings.h"
#include "streaming/rendering/StreamSwapStallWatchdog.h"
#include "streaming/rendering/StreamVideoRenderCallback.h"
#include "streaming/rendering/StreamVideoTextureRenderer.h"
#include "streaming/rendering/StreamFrameInterpolator.h"
#include "streaming/rendering/StreamFramePacer.h"

#include <rhi/qrhi.h>
#include <rhi/qrhi_platform.h>
#include <QDebug>

#include <utility>
#include <atomic>
#include <algorithm>
#include <chrono>
#include <vector>
#if defined(Q_OS_WIN)
#include <d3d11.h>
#include <wrl/client.h>
#endif

namespace {
class ExternalCommandScope final
{
public:
    explicit ExternalCommandScope(QRhiCommandBuffer *commandBuffer)
        : m_commandBuffer(commandBuffer)
    {
        m_commandBuffer->beginExternal();
    }

    ~ExternalCommandScope()
    {
        m_commandBuffer->endExternal();
    }

    ExternalCommandScope(const ExternalCommandScope &) = delete;
    ExternalCommandScope &operator=(const ExternalCommandScope &) = delete;

private:
    QRhiCommandBuffer *m_commandBuffer;
};

#if defined(Q_OS_WIN)
// GPU timing for the streamvideo downscale draw. QRhi's D3D11 backend
// executes command-buffer draws on the adopted immediate context as they are
// recorded, so raw timestamp queries bracketing the draw measure the pass's
// GPU time. Each slot is read back eight frames later with
// D3D11_ASYNC_GETDATA_DONOTFLUSH; a result that is not ready is skipped,
// never waited on, so diagnostics can never stall the render thread.
class D3D11DownscaleGpuTimer final
{
public:
    static constexpr int Ring = 8;

    struct Stats
    {
        qint64 p50Us = 0;
        qint64 p95Us = 0;
        qint64 maxUs = 0;
        int n = 0;
    };

    void initialize(ID3D11Device *device, ID3D11DeviceContext *context)
    {
        release();
        m_device = device;
        m_context = context;
        if (!m_device || !m_context) return;
        for (int slot = 0; slot < Ring; ++slot) {
            m_disjoint[slot] = create(D3D11_QUERY_TIMESTAMP_DISJOINT);
            m_start[slot] = create(D3D11_QUERY_TIMESTAMP);
            m_end[slot] = create(D3D11_QUERY_TIMESTAMP);
        }
    }

    void release()
    {
        for (int slot = 0; slot < Ring; ++slot) {
            m_disjoint[slot].Reset();
            m_start[slot].Reset();
            m_end[slot].Reset();
            m_armed[slot] = false;
        }
        m_cursor = 0;
        m_device = nullptr;
        m_context = nullptr;
        m_samples.clear();
    }

    void begin()
    {
        if (!m_context) return;
        const int slot = m_cursor;
        collect(slot);
        if (!m_disjoint[slot] || !m_start[slot] || !m_end[slot]) return;
        m_context->Begin(m_disjoint[slot].Get());
        m_context->End(m_start[slot].Get());
        m_armed[slot] = true;
    }

    void end()
    {
        if (!m_context) return;
        const int slot = m_cursor;
        if (!m_armed[slot]) return;
        m_armed[slot] = false;
        if (m_end[slot] && m_disjoint[slot]) {
            m_context->End(m_end[slot].Get());
            m_context->End(m_disjoint[slot].Get());
        }
        m_cursor = (slot + 1) % Ring;
    }

    Stats stats() const
    {
        Stats result;
        result.n = int(m_samples.size());
        if (m_samples.empty()) return result;
        std::vector<qint64> sorted = m_samples;
        std::sort(sorted.begin(), sorted.end());
        const auto at = [&sorted](double fraction) {
            const size_t index = size_t(sorted.size() * fraction);
            return sorted[index < sorted.size() ? index : sorted.size() - 1];
        };
        result.p50Us = at(0.50);
        result.p95Us = at(0.95);
        result.maxUs = sorted.back();
        return result;
    }

private:
    Microsoft::WRL::ComPtr<ID3D11Query> create(D3D11_QUERY type)
    {
        Microsoft::WRL::ComPtr<ID3D11Query> query;
        if (!m_device) return query;
        D3D11_QUERY_DESC description{};
        description.Query = type;
        m_device->CreateQuery(&description, &query);
        return query;
    }

    void collect(int slot)
    {
        // Only resolve a slot whose previous cycle closed; GetData with
        // DONOTFLUSH returns S_FALSE while pending and leaves the buffers
        // untouched, so the zeroed sentinel decides.
        if (!m_context || m_armed[slot]) return;
        if (!m_disjoint[slot] || !m_start[slot] || !m_end[slot]) return;
        D3D11_QUERY_DATA_TIMESTAMP_DISJOINT disjoint{};
        UINT64 start = 0;
        UINT64 end = 0;
        const auto flags = D3D11_ASYNC_GETDATA_DONOTFLUSH;
        if (m_context->GetData(m_disjoint[slot].Get(), &disjoint, sizeof(disjoint), flags) != S_OK)
            return;
        if (m_context->GetData(m_start[slot].Get(), &start, sizeof(start), flags) != S_OK)
            return;
        if (m_context->GetData(m_end[slot].Get(), &end, sizeof(end), flags) != S_OK)
            return;
        if (disjoint.Frequency == 0 || disjoint.Disjoint || start == 0 || end < start)
            return;
        const UINT64 micros = (end - start) * 1000000ull / disjoint.Frequency;
        if (m_samples.size() >= 256)
            m_samples.erase(m_samples.begin());
        m_samples.push_back(qint64(micros));
    }

    ID3D11Device *m_device = nullptr;
    ID3D11DeviceContext *m_context = nullptr;
    Microsoft::WRL::ComPtr<ID3D11Query> m_disjoint[Ring];
    Microsoft::WRL::ComPtr<ID3D11Query> m_start[Ring];
    Microsoft::WRL::ComPtr<ID3D11Query> m_end[Ring];
    bool m_armed[Ring] = {};
    int m_cursor = 0;
    std::vector<qint64> m_samples;
};
#endif

class NativeStreamRenderCallback final : public StreamVideoRenderCallback
{
    enum class FrameGenerationState { Off, WarmingUp, Active, DisplayTooSlow, Overloaded, Unavailable, Discontinuity, SourceRateLimit, HdrUnsupported };
public:
    explicit NativeStreamRenderCallback(NativeStreamRuntime *runtime)
        : m_runtime(runtime)
    {
    }

    void initialize(QRhi *rhi, QRhiCommandBuffer *commandBuffer,
                    QRhiRenderTarget *renderTarget) override
    {
        const bool ownRearm = std::exchange(m_resourceRearmPending, false);
        const bool deviceChanged = m_rhi != rhi;
        const bool generationChanged = m_runtime
            && m_presentationGeneration != m_runtime->presentationGeneration();
        if (ownRearm || deviceChanged || generationChanged) {
            tearDownResources(ownRearm, deviceChanged, generationChanged);
            if (generationChanged)
                m_presentationGeneration = m_runtime->presentationGeneration();
        }
        m_textures.initialize(rhi, renderTarget);
        if (m_rhi == rhi && m_graphicsReady) return;
        m_rhi = rhi;
        if (!commandBuffer) return;

        OpenNowStreamerGraphicsContext context{};
        context.version = OPENNOW_STREAMER_GRAPHICS_CONTEXT_VERSION;
        context.struct_size = sizeof(context);
        OpenNowStreamerStatus status = OPENNOW_STREAMER_GRAPHICS_UNAVAILABLE;
        {
            // The native streamer creates decoder/video-processor resources on Qt's adopted
            // graphics device. QRhi requires every external native command to be bracketed so it
            // can invalidate and restore its internal backend state.
            ExternalCommandScope externalCommands(commandBuffer);
            switch (rhi->backend()) {
#if defined(Q_OS_WIN)
            case QRhi::D3D11: {
                const auto *handles = static_cast<const QRhiD3D11NativeHandles *>(
                    rhi->nativeHandles());
                if (!handles) return;
                context.graphics_api = OPENNOW_STREAMER_GRAPHICS_API_D3D11;
                context.device = handles->dev;
                context.queue = handles->context;
                // The public QRhi native-handles header type-erases these to
                // void*; the D3D11 backend always hands out the real
                // device/immediate-context pointers.
                m_downscaleGpu.initialize(static_cast<ID3D11Device *>(handles->dev),
                                          static_cast<ID3D11DeviceContext *>(handles->context));
                break;
            }
#endif
#if QT_CONFIG(vulkan) && __has_include(<vulkan/vulkan.h>)
            case QRhi::Vulkan: {
                const auto *handles = static_cast<const QRhiVulkanNativeHandles *>(
                    rhi->nativeHandles());
                if (!handles || !handles->inst) return;
                context.graphics_api = OPENNOW_STREAMER_GRAPHICS_API_VULKAN;
                context.instance = reinterpret_cast<void *>(handles->inst->vkInstance());
                context.physical_device = reinterpret_cast<void *>(handles->physDev);
                context.device = reinterpret_cast<void *>(handles->dev);
                context.queue = reinterpret_cast<void *>(handles->gfxQueue);
                context.queue_family_index = handles->gfxQueueFamilyIdx;
#if defined(Q_OS_LINUX)
                if (m_runtime && m_runtime->vulkanDevice()) {
                    OpenNowStreamerVulkanDeviceInfo info{};
                    info.version = OPENNOW_STREAMER_VULKAN_DEVICE_INFO_VERSION;
                    info.struct_size = sizeof(info);
                    if (opennow_streamer_vulkan_device_info(m_runtime->vulkanDevice(), &info)
                            != OPENNOW_STREAMER_OK
                            || !LinuxVulkanGraphics::Device::matchesContext(info, context)) {
                        reportFailure(QStringLiteral("Qt is not using the embedded Vulkan Video device. Restart OpenNOW to recreate the shared graphics device."));
                        return;
                    }
                } else {
                    context.enabled_capabilities = LinuxVulkanGraphics::enabledImportCapabilities(
                        handles->inst, handles->physDev);
                }
#endif
                break;
            }
#endif
#if QT_CONFIG(metal)
            case QRhi::Metal: {
                const auto *handles = static_cast<const QRhiMetalNativeHandles *>(
                    rhi->nativeHandles());
                if (!handles) return;
                context.graphics_api = OPENNOW_STREAMER_GRAPHICS_API_METAL;
                context.device = handles->dev;
                context.queue = handles->cmdQueue;
                break;
            }
#endif
            default:
                reportFailure(QStringLiteral("This graphics backend cannot present native video. Use Auto in Stream settings."));
                return;
            }
            status = m_runtime ? m_runtime->setGraphicsContext(context)
                               : OPENNOW_STREAMER_CLOSED;
        }
        m_graphicsReady = status == OPENNOW_STREAMER_OK;
        if (!m_graphicsReady)
            reportFailure(QStringLiteral("Could not initialize native graphics (status %1). Update the GPU driver and use Auto in Stream settings.").arg(int(status)));
    }

    void prepareFrame(QRhiCommandBuffer *commandBuffer) override
    {
        prepareNativeFrame(commandBuffer);
        if (!m_runtime || !m_runtime->presentationAllowed()
            || m_presentationGeneration != m_runtime->presentationGeneration()
            || !m_graphicsReady || !m_rhi || !commandBuffer) return;
        m_textures.prepareUpscaling(commandBuffer, m_upscalingTarget, m_fsrUpscaling,
            m_sourceColorSpace == OPENNOW_STREAMER_COLOR_SPACE_SDR709, m_upscalingSharpness);
    }

    void prepareNativeFrame(QRhiCommandBuffer *commandBuffer)
    {
        if (!m_runtime || !m_runtime->presentationAllowed()
                || m_presentationGeneration != m_runtime->presentationGeneration()) return;
        if (!m_graphicsReady || !m_rhi || !commandBuffer) return;
        if (m_resetFrameGeneration) {
            resetFrameGeneration();
            m_resetFrameGeneration = false;
        }
        const auto output = HdrOutput::renderState();
        m_textures.setColorSpace(m_sourceColorSpace, output.mode, output.whiteNits, output.supported);
        {
            // off / low / medium / high -> shader strength
            static const float strengths[] = {0.0f, 0.15f, 0.30f, 0.45f};
            const auto index = std::clamp(m_downscaleSharpen, 0, 3);
            m_textures.setDownscaleSharpen(m_downscaleHq, strengths[index]);
        }
        if (!m_textures.prepare(commandBuffer)) {
            reportFailure(QStringLiteral("Could not create the video shaders or GPU resources. Check the packaged shaders and GPU driver."));
            return;
        }

        OpenNowStreamerRecordCommand command{};
        command.version = OPENNOW_STREAMER_RENDER_COMMAND_VERSION;
        command.struct_size = sizeof(command);
        command.frame_slot = static_cast<std::uint32_t>(m_rhi->currentFrameSlot());
        if (m_rhi->backend() == QRhi::Metal && !m_fsrUpscaling && !m_upscalingTarget.isEmpty()) {
            command.upscale_width = static_cast<std::uint32_t>(m_upscalingTarget.width());
            command.upscale_height = static_cast<std::uint32_t>(m_upscalingTarget.height());
            command.upscale_sharpness = static_cast<std::uint32_t>(m_upscalingSharpness);
            command.upscale_denoise = static_cast<std::uint32_t>(m_upscalingDenoise);
        }
        finishFrame();
        OpenNowStreamerFrameInfo info{};
        OpenNowStreamerRecordedFrame recorded{};
        OpenNowStreamerFrame *frame = nullptr;
        OpenNowStreamerStatus status = OPENNOW_STREAMER_GRAPHICS_UNAVAILABLE;
        {
            ExternalCommandScope externalCommands(commandBuffer);
            // Native command-buffer handles can change when QRhi begins an external section, so
            // query them only after beginExternal(), as required by the QRhi contract.
            switch (m_rhi->backend()) {
#if defined(Q_OS_WIN)
            case QRhi::D3D11: {
                const auto *handles = static_cast<const QRhiD3D11NativeHandles *>(
                    m_rhi->nativeHandles());
                command.command_buffer = handles ? handles->context : nullptr;
                break;
            }
#endif
#if QT_CONFIG(vulkan) && __has_include(<vulkan/vulkan.h>)
            case QRhi::Vulkan: {
                const auto *handles = static_cast<const QRhiVulkanCommandBufferNativeHandles *>(
                    commandBuffer->nativeHandles());
                command.command_buffer = handles
                    ? reinterpret_cast<void *>(handles->commandBuffer) : nullptr;
                break;
            }
#endif
#if QT_CONFIG(metal)
            case QRhi::Metal: {
                const auto *handles = static_cast<const QRhiMetalCommandBufferNativeHandles *>(
                    commandBuffer->nativeHandles());
                command.command_buffer = handles ? handles->commandBuffer : nullptr;
                break;
            }
#endif
            default:
                return;
            }
            if (!command.command_buffer) return;
            status = m_runtime->recordLatestFrame(command, &info, &recorded, &frame);
        }

        if (m_presentationGeneration != m_runtime->presentationGeneration()
                || !m_runtime->presentationAllowed()) {
            if (frame) m_runtime->releaseFrame(frame);
            m_textures.clearFrames();
            return;
        }
        if (status == OPENNOW_STREAMER_NO_FRAME || status == OPENNOW_STREAMER_STALE_FRAME) {
            prepareOriginal();
            return;
        }
        if (status != OPENNOW_STREAMER_OK || !frame || recorded.resource == 0
            || recorded.width == 0 || recorded.height == 0
            || (recorded.texture_format != OPENNOW_STREAMER_TEXTURE_FORMAT_RGBA8
                && recorded.texture_format != OPENNOW_STREAMER_TEXTURE_FORMAT_RGB10A2
                && recorded.texture_format != OPENNOW_STREAMER_TEXTURE_FORMAT_RGBA16F)
            || (recorded.color_space != OPENNOW_STREAMER_COLOR_SPACE_SDR709
                && recorded.texture_format == OPENNOW_STREAMER_TEXTURE_FORMAT_RGBA8)
            || recorded.color_space > OPENNOW_STREAMER_COLOR_SPACE_HLG2020) {
            if (frame) m_runtime->releaseFrame(frame);
            reportFailure(QStringLiteral("Could not present the decoded GPU frame (status %1). Check native-streamer.log for decoder or device errors.").arg(int(status)));
            return;
        }
        m_preparedFrame = frame;
        if (m_sourceColorSpace != int(recorded.color_space)) {
            resetFrameGeneration();
            m_sourceColorSpace = int(recorded.color_space);
            m_textures.updateColorSpace(commandBuffer, m_sourceColorSpace);
        }

        QRhiTexture::NativeTexture texture{};
        texture.object = recorded.resource;
#if QT_CONFIG(vulkan) && __has_include(<vulkan/vulkan.h>)
        if (recorded.graphics_api == OPENNOW_STREAMER_GRAPHICS_API_VULKAN)
            texture.layout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
#endif
        if (!m_textures.importFrame(command.frame_slot, texture,
            recorded.texture_format == OPENNOW_STREAMER_TEXTURE_FORMAT_RGB10A2
                ? QRhiTexture::RGB10A2
                : recorded.texture_format == OPENNOW_STREAMER_TEXTURE_FORMAT_RGBA16F
                    ? QRhiTexture::RGBA16F : QRhiTexture::RGBA8,
            QSize(int(recorded.width), int(recorded.height)))) {
            reportFailure(QStringLiteral("Could not import the decoded video texture into Qt. The decoder and graphics backend must use compatible GPU resources."));
            return;
        }
        if (m_reportedColorFormat != recorded.texture_format
                || m_reportedColorSpace != recorded.color_space
                || m_reportedOutputBits != m_textures.outputBits()) {
            m_reportedColorFormat = recorded.texture_format;
            m_reportedColorSpace = recorded.color_space;
            m_reportedOutputBits = m_textures.outputBits();
            qInfo("Video composition: sourceColorSpace=%s textureBits=%u outputBits=%d dither=%s",
                  recorded.color_space == OPENNOW_STREAMER_COLOR_SPACE_PQ2020 ? "PQ2020"
                    : recorded.color_space == OPENNOW_STREAMER_COLOR_SPACE_HLG2020 ? "HLG2020" : "SDR709",
                  recorded.texture_format == OPENNOW_STREAMER_TEXTURE_FORMAT_RGBA16F ? 16u
                    : recorded.texture_format == OPENNOW_STREAMER_TEXTURE_FORMAT_RGB10A2 ? 10u : 8u,
                  m_reportedOutputBits, m_reportedOutputBits == 8 ? "ordered-8x8" : "none");
        }
        m_outputDirty = true;
        m_outputKind = 1;
        if (m_sourceColorSpace != OPENNOW_STREAMER_COLOR_SPACE_SDR709) {
            m_frameGenerationStatus.store(m_frameGeneration ? FrameGenerationState::HdrUnsupported : FrameGenerationState::Off);
            return;
        }
        if (!m_frameGeneration || m_frameGenerationFailed) return;

        const auto now = clockNs();
        const auto decision = m_pacer.source(info.sequence, info.presentation_time_ns, now, m_refreshRate);
        updateTimingStats();
        if (decision == StreamFramePacer::Result::Duplicate) {
            m_outputDirty = false;
            if (m_pacer.pending())
                m_textures.selectTexture(m_interpolator.midpointTexture());
            else if (m_interpolator.hasPair())
                m_textures.selectTexture(m_interpolator.currentTexture());
            prepareOriginal();
            return;
        }
        m_needsFrame.store(false);
        if (decision != StreamFramePacer::Result::Interpolate) {
            m_interpolator.reset();
            switch (decision) {
            case StreamFramePacer::Result::SourceRateLimit: m_frameGenerationStatus.store(FrameGenerationState::SourceRateLimit); return;
            case StreamFramePacer::Result::DisplayTooSlow: m_frameGenerationStatus.store(FrameGenerationState::DisplayTooSlow); return;
            case StreamFramePacer::Result::Overloaded: m_frameGenerationStatus.store(FrameGenerationState::Overloaded); return;
            case StreamFramePacer::Result::Discontinuity: m_frameGenerationStatus.store(FrameGenerationState::Discontinuity); break;
            default: m_frameGenerationStatus.store(FrameGenerationState::WarmingUp); break;
            }
        }
        auto *source = m_textures.importedTexture();
        if (m_historySize != source->pixelSize() || m_historyFormat != source->format()) {
            m_textures.clearExternalTextures();
            m_interpolator.release();
            m_historySize = source->pixelSize();
            m_historyFormat = source->format();
        }
        if (!m_interpolator.initialize(m_rhi) || !m_interpolator.ingest(commandBuffer, source)) {
            disableFrameGeneration();
            return;
        }
        if (!m_interpolator.hasPair()) {
            if (decision != StreamFramePacer::Result::Discontinuity)
                m_frameGenerationStatus.store(FrameGenerationState::WarmingUp);
            return;
        }
        if (!m_textures.selectTexture(m_interpolator.midpointTexture())) {
            disableFrameGeneration();
            return;
        }
        m_midpointSwapped.store(false);
        m_pacer.midpoint(clockNs());
        m_needsFrame.store(true);
        m_outputKind = 2;
        m_frameGenerationStatus.store(FrameGenerationState::Active);
    }

    void setUpscalingTarget(const QSize &size) override
    {
        const auto target = size.width() > 0 && size.height() > 0
                && size.width() <= 16384 && size.height() <= 16384 ? size : QSize();
        if (m_upscalingTarget != target && m_rhi && m_rhi->backend() == QRhi::Metal)
            m_resetFrameGeneration = true;
        m_upscalingTarget = target;
    }

    void setFsrUpscaling(bool enabled) override
    {
        m_fsrUpscaling = enabled;
    }

    void setDownscaleSharpen(bool hq, int sharpen) override
    {
        m_downscaleHq = hq;
        m_downscaleSharpen = sharpen;
    }

    void setUpscalingEnhancement(int sharpness, int denoise) override
    {
        sharpness = qBound(0, sharpness, 15);
        denoise = qBound(0, denoise, 20);
        if (m_upscalingSharpness == sharpness && m_upscalingDenoise == denoise) return;
        m_upscalingSharpness = sharpness;
        m_upscalingDenoise = denoise;
        if (!m_upscalingTarget.isEmpty() && m_rhi && m_rhi->backend() == QRhi::Metal)
            m_resetFrameGeneration = true;
    }

    void setFrameGeneration(bool enabled, double refreshRate) override
    {
        if (m_frameGeneration != enabled || m_refreshRate != refreshRate)
            m_resetFrameGeneration = true;
        m_frameGeneration = enabled;
        m_refreshRate = refreshRate;
        m_reportedRefreshRate.store(refreshRate);
    }

    bool needsFrame() const override
    {
        return m_needsFrame.load() && m_runtime && m_runtime->presentationAllowed();
    }

    QVariantMap swapStats() const override
    {
        const auto snapshot = m_swapTimings.snapshot();
        QVariantMap stats;
        stats.insert(QStringLiteral("available"), snapshot.available);
        if (snapshot.available) {
            stats.insert(QStringLiteral("p50Ms"), double(snapshot.submitToSwap.p50Ns) / 1.0e6);
            stats.insert(QStringLiteral("p95Ms"), double(snapshot.submitToSwap.p95Ns) / 1.0e6);
            stats.insert(QStringLiteral("maxMs"), double(snapshot.submitToSwap.maxNs) / 1.0e6);
        }
        stats.insert(QStringLiteral("windowSamples"), int(snapshot.windowSamples));
        stats.insert(QStringLiteral("swappedFramesTotal"),
                     qulonglong(snapshot.swappedFramesTotal));
        stats.insert(QStringLiteral("epoch"), qulonglong(snapshot.epoch));
        if (snapshot.hasLastSwap)
            stats.insert(QStringLiteral("sinceLastSwapMs"),
                         double(clockNs() - snapshot.lastSwapNs) / 1.0e6);
#if defined(Q_OS_WIN)
        const auto gpu = m_downscaleGpu.stats();
        QVariantMap gpuStats;
        gpuStats.insert(QStringLiteral("p50Us"), gpu.p50Us);
        gpuStats.insert(QStringLiteral("p95Us"), gpu.p95Us);
        gpuStats.insert(QStringLiteral("maxUs"), gpu.maxUs);
        gpuStats.insert(QStringLiteral("n"), gpu.n);
        stats.insert(QStringLiteral("gpuDownscaleUs"), gpuStats);
#else
        stats.insert(QStringLiteral("gpuDownscaleUs"), QVariantMap());
#endif
        return stats;
    }

    void setSwapGated(bool gated, const QString &) override
    {
        m_swapGateActive = gated;
        m_swapTimings.setGated(gated);
        if (gated) m_swapStall.reset();
    }

    void frameSwapped() override
    {
        const int kind = m_submittedKind.exchange(0);
        if (kind) {
            ++m_outputCount;
            m_swapTimings.markSwap(clockNs());
        }
        if (kind == 2) m_midpointSwapped.store(true);
        const auto now = clockNs();
        const auto start = m_sampleStart.load();
        if (!start) {
            m_sampleStart.store(now);
            m_outputCount.store(0);
        } else if (now - start >= 1'000'000'000) {
            m_outputFps.store(double(m_outputCount.exchange(0)) * 1.0e9 / double(now - start));
            m_sampleStart.store(now);
        }
    }

    QVariantMap frameGenerationStats() const override
    {
        const QString states[] = {QStringLiteral("off"), QStringLiteral("warming-up"),
            QStringLiteral("active"), QStringLiteral("display-refresh"),
            QStringLiteral("overloaded"), QStringLiteral("unavailable"), QStringLiteral("discontinuity"),
            QStringLiteral("source-rate-limit"), QStringLiteral("hdr-unavailable")};
        const QString timing[] = {QStringLiteral("none"), QStringLiteral("source-timestamps"),
                                  QStringLiteral("arrival-cadence")};
        const QString rejections[] = {QStringLiteral("none"), QStringLiteral("sequence-gap"),
            QStringLiteral("timestamp-regression"), QStringLiteral("timestamp-jump"),
            QStringLiteral("arrival-gap"), QStringLiteral("cadence-unavailable")};
        return {{QStringLiteral("status"), states[int(m_frameGenerationStatus.load())]},
                {QStringLiteral("timingSource"), timing[int(m_timingSource.load())]},
                {QStringLiteral("rejectionReason"), rejections[int(m_rejection.load())]},
                {QStringLiteral("sourceIntervalMs"), double(m_sourceInterval.load()) / 1.0e6},
                {QStringLiteral("timestampDeltaMs"), double(m_timestampDelta.load()) / 1.0e6},
                {QStringLiteral("arrivalDeltaMs"), double(m_arrivalDelta.load()) / 1.0e6},
                {QStringLiteral("sequenceDelta"), qulonglong(m_sequenceDelta.load())},
                {QStringLiteral("refreshRateHz"), m_reportedRefreshRate.load()},
                {QStringLiteral("outputFps"), clockNs() - m_sampleStart.load() < 2'000'000'000
                    ? m_outputFps.load() : 0.0}};
    }

    void setComposition(const QMatrix4x4 &matrix, const QRectF &bounds,
                        const QRectF &videoRect, float opacity) override
    {
        m_textures.setComposition(matrix, bounds, videoRect, opacity);
    }

    void setClip(bool stencil, int reference) override
    {
        m_stencil = stencil;
        m_stencilReference = reference;
    }

    void recordFrame(QRhiCommandBuffer *commandBuffer, const QRect &) override
    {
        if (!m_runtime || !m_runtime->presentationAllowed()
                || m_presentationGeneration != m_runtime->presentationGeneration()) return;
#if defined(Q_OS_WIN)
        m_downscaleGpu.begin();
#endif
        m_textures.render(commandBuffer, m_stencil, m_stencilReference);
#if defined(Q_OS_WIN)
        m_downscaleGpu.end();
#endif
        if (m_outputDirty) {
            m_submittedKind.store(m_outputKind);
            m_outputDirty = false;
            if (m_outputKind == 1) m_swapTimings.markSubmit(clockNs());
        }
        observeSwapProgress();
    }

    void finishFrame() override
    {
        if (m_preparedFrame && m_runtime)
            m_runtime->releaseFrame(std::exchange(m_preparedFrame, nullptr));
    }

    void releaseResources() override
    {
        tearDownResources(false, false, false);
    }

    void tearDownResources(bool ownRearm, bool deviceChanged, bool generationChanged)
    {
        m_swapStall.onResourcesReleased(ownRearm, deviceChanged, generationChanged);
        if (m_rhi && m_graphicsReady) m_rhi->finish();
        finishFrame();
        m_textures.release();
        m_interpolator.release();
        m_pacer.reset();
        m_downscaleGpu.release();
        m_swapTimings.reset();
        updateTimingStats();
        m_needsFrame.store(false);
        m_submittedKind.store(0);
        m_outputCount.store(0);
        m_outputFps.store(0);
        m_sampleStart.store(0);
        m_outputDirty = false;
        m_midpointSwapped.store(false);
        m_resetFrameGeneration = true;
        if (m_rhi && m_graphicsReady) m_rhi->finish();
        if (m_graphicsReady && m_runtime) m_runtime->sceneGraphShutdown();
        m_graphicsReady = false;
        m_reportedFailure = false;
        m_reportedColorFormat = 0;
        m_reportedColorSpace = 0;
        m_reportedOutputBits = 0;
        m_rhi = nullptr;
    }

    void observeSwapProgress()
    {
        const auto snapshot = m_swapTimings.snapshot();
        const auto now = clockNs();
        StreamSwapStallWatchdog::Observation observation;
        observation.gated = m_swapGateActive;
        observation.hasPendingSubmit = snapshot.hasPendingSubmit;
        observation.hasLastSwap = snapshot.hasLastSwap;
        observation.lastSwapNs = snapshot.lastSwapNs;
        const auto progress = m_runtime
            ? m_runtime->upstreamProgress() : NativeStreamRuntime::UpstreamProgress{};
        observation.upstreamStalled = progress.stalled;
        observation.hasUpstreamSample = progress.hasDecodeTimings;
        observation.upstreamEpoch = progress.decodeEpoch;
        observation.upstreamOutputsTotal = progress.decodedOutputsTotal;
        const auto outcome = m_swapStall.observe(observation, now);
        if (outcome == StreamSwapStallWatchdog::Outcome::ResourceRearm) {
            m_resourceRearmPending = true;
        } else if (outcome == StreamSwapStallWatchdog::Outcome::Unrecovered) {
            reportFailure(QStringLiteral("The render thread stopped swapping decoded frames. The last presentation resource re-arm did not restore the stream, so the session must reconnect."));
        }
    }

private:
    NativeStreamRuntime *m_runtime;
    QRhi *m_rhi = nullptr;
    std::uint32_t m_reportedColorFormat = 0;
    std::uint32_t m_reportedColorSpace = 0;
    int m_reportedOutputBits = 0;
    StreamVideoTextureRenderer m_textures;
    StreamPresentTimings m_swapTimings;
#if defined(Q_OS_WIN)
    D3D11DownscaleGpuTimer m_downscaleGpu;
#endif
    StreamSwapStallWatchdog m_swapStall;
    bool m_swapGateActive = false;
    bool m_resourceRearmPending = false;
    int m_sourceColorSpace = OPENNOW_STREAMER_COLOR_SPACE_SDR709;
    StreamFrameInterpolator m_interpolator;
    StreamFramePacer m_pacer;
    QSize m_historySize;
    QRhiTexture::Format m_historyFormat = QRhiTexture::UnknownFormat;
    double m_refreshRate = 0;
    bool m_frameGeneration = false;
    QSize m_upscalingTarget;
    bool m_fsrUpscaling = false;
    int m_upscalingSharpness = 10;
    int m_upscalingDenoise = 0;
    bool m_downscaleHq = true;
    int m_downscaleSharpen = 1;
    bool m_frameGenerationFailed = false;
    bool m_resetFrameGeneration = false;
    bool m_outputDirty = false;
    int m_outputKind = 0;
    std::atomic_bool m_needsFrame = false;
    std::atomic_bool m_midpointSwapped = false;
    std::atomic_int m_submittedKind = 0;
    std::atomic<FrameGenerationState> m_frameGenerationStatus = FrameGenerationState::Off;
    std::atomic_uint64_t m_outputCount = 0;
    std::atomic_int64_t m_sampleStart = 0;
    std::atomic<double> m_outputFps = 0;
    std::atomic<StreamFramePacer::TimingSource> m_timingSource = StreamFramePacer::TimingSource::None;
    std::atomic<StreamFramePacer::Rejection> m_rejection = StreamFramePacer::Rejection::None;
    std::atomic_uint64_t m_sourceInterval = 0;
    std::atomic_uint64_t m_timestampDelta = 0;
    std::atomic_uint64_t m_sequenceDelta = 0;
    std::atomic_int64_t m_arrivalDelta = 0;
    std::atomic<double> m_reportedRefreshRate = 0;
    OpenNowStreamerFrame *m_preparedFrame = nullptr;
    int m_stencilReference = 0;
    bool m_stencil = false;
    bool m_graphicsReady = false;
    bool m_reportedFailure = false;
    quint64 m_presentationGeneration = 0;
    static std::int64_t clockNs()
    {
        return streamMonotonicClockNs();
    }

    void updateTimingStats()
    {
        m_timingSource.store(m_pacer.timingSource());
        m_rejection.store(m_pacer.rejection());
        m_sourceInterval.store(m_pacer.interval());
        m_timestampDelta.store(m_pacer.timestampDelta());
        m_sequenceDelta.store(m_pacer.sequenceDelta());
        m_arrivalDelta.store(m_pacer.arrivalDelta());
    }

    void resetFrameGeneration()
    {
        m_textures.clearExternalTextures();
        m_interpolator.release();
        m_pacer.reset();
        updateTimingStats();
        m_needsFrame.store(false);
        m_submittedKind.store(0);
        m_midpointSwapped.store(false);
        m_outputDirty = false;
        m_frameGenerationFailed = false;
        m_frameGenerationStatus.store(m_frameGeneration ? FrameGenerationState::WarmingUp : FrameGenerationState::Off);
    }

    void disableFrameGeneration()
    {
        m_textures.clearExternalTextures();
        m_interpolator.release();
        m_pacer.reset();
        updateTimingStats();
        m_needsFrame.store(false);
        m_frameGenerationFailed = true;
        m_frameGenerationStatus.store(FrameGenerationState::Unavailable);
    }

    void prepareOriginal()
    {
        if (!m_frameGeneration || !m_midpointSwapped.load()
            || !m_pacer.takeOriginal(clockNs(), m_refreshRate)) return;
        m_needsFrame.store(false);
        if (!m_textures.selectTexture(m_interpolator.currentTexture())) {
            disableFrameGeneration();
            return;
        }
        m_outputDirty = true;
        m_outputKind = 1;
    }
    void reportFailure(const QString &message)
    {
        if (m_reportedFailure || !m_runtime) return;
        m_reportedFailure = true;
        m_runtime->reportPresentationError(message, m_presentationGeneration);
    }
};
}

std::shared_ptr<StreamVideoRenderCallback> createNativeStreamRenderCallback(
    NativeStreamRuntime *runtime)
{
    return std::make_shared<NativeStreamRenderCallback>(runtime);
}
