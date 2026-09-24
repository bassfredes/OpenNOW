#include "streaming/StreamVideoItem.h"

#include "input/platform/WaylandPointerCapture.h"
#include "input/platform/MacPointerCapture.h"
#include "streaming/NativeStreamRuntime.h"

#include <QCursor>
#include <QClipboard>
#include <QFocusEvent>
#include <QGuiApplication>
#include <QHoverEvent>
#include <QKeyEvent>
#include <QKeySequence>
#include <QMouseEvent>
#include <QPixmap>
#include <QQuickWindow>
#include <QWheelEvent>

#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <utility>

#if defined(Q_OS_WIN)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#endif

namespace {

// Bounded event->submit stage timing for the input path, mirroring the native
// stage_timing windows: the last 240 samples, one payload-free p50/p95 line
// every five seconds on stderr. It measures the synchronous GUI-thread leg,
// handler entry -> FFI submit return. Residence inside the OS message queue
// before Qt sees the event is not observable in-process; Windows coalesces
// WM_MOUSEMOVE there for every client, including the official one.
using InputEventClock = std::chrono::steady_clock;
constexpr std::size_t InputTimingSamples = 240;

std::array<qint64, InputTimingSamples> g_inputTimingUs{};
std::size_t g_inputTimingCount = 0;
std::size_t g_inputTimingNext = 0;
qint64 g_inputTimingMaxUs = 0;
InputEventClock::time_point g_inputTimingReportAt{};

InputEventClock::time_point inputEventBegin()
{
    const auto now = InputEventClock::now();
    if (g_inputTimingReportAt.time_since_epoch().count() == 0)
        g_inputTimingReportAt = now + std::chrono::seconds(5);
    return now;
}

void inputEventSubmitted(InputEventClock::time_point begun)
{
    const auto us = std::chrono::duration_cast<std::chrono::microseconds>(
                        InputEventClock::now() - begun)
                        .count();
    g_inputTimingUs[g_inputTimingNext] = us;
    g_inputTimingNext = (g_inputTimingNext + 1) % InputTimingSamples;
    if (g_inputTimingCount < InputTimingSamples) ++g_inputTimingCount;
    g_inputTimingMaxUs = std::max(g_inputTimingMaxUs, static_cast<qint64>(us));
    const auto now = InputEventClock::now();
    if (now < g_inputTimingReportAt) return;
    std::array<qint64, InputTimingSamples> sorted{};
    std::copy_n(g_inputTimingUs.data(), g_inputTimingCount, sorted.data());
    std::sort(sorted.begin(), sorted.begin() + g_inputTimingCount);
    const auto count = static_cast<qint64>(g_inputTimingCount);
    qWarning("input-stage-timings eventToSubmit p50Us=%lld p95Us=%lld maxUs=%lld n=%lld",
             sorted[(g_inputTimingCount - 1) / 2],
             sorted[(g_inputTimingCount * 95 + 99) / 100 - 1], g_inputTimingMaxUs, count);
    g_inputTimingCount = 0;
    g_inputTimingNext = 0;
    g_inputTimingMaxUs = 0;
    g_inputTimingReportAt = now + std::chrono::seconds(5);
}

} // namespace

quint16 StreamVideoItem::windowsVirtualKey(int key, Qt::KeyboardModifiers modifiers,
                                          quint32 nativeVirtualKey)
{
    if (nativeVirtualKey > 0 && nativeVirtualKey < 0xff) {
        switch (nativeVirtualKey) {
        case 0x10: return 0xa0;
        case 0x11: return 0xa2;
        case 0x12: return 0xa4;
        default: return static_cast<quint16>(nativeVirtualKey);
        }
    }
    if (key >= Qt::Key_A && key <= Qt::Key_Z) return static_cast<quint16>(key);
    if (key >= Qt::Key_0 && key <= Qt::Key_9) return static_cast<quint16>(key);
    if (key >= Qt::Key_F1 && key <= Qt::Key_F24)
        return static_cast<quint16>(0x70 + key - Qt::Key_F1);
    switch (key) {
    case Qt::Key_Return:
    case Qt::Key_Enter: return 0x0d;
    case Qt::Key_Escape: return 0x1b;
    case Qt::Key_Backspace: return 0x08;
    case Qt::Key_Tab:
    case Qt::Key_Backtab: return 0x09;
    case Qt::Key_Space: return 0x20;
    case Qt::Key_Exclam: return 0x31;
    case Qt::Key_At: return 0x32;
    case Qt::Key_NumberSign: return 0x33;
    case Qt::Key_Dollar: return 0x34;
    case Qt::Key_Percent: return 0x35;
    case Qt::Key_AsciiCircum: return 0x36;
    case Qt::Key_Ampersand: return 0x37;
    case Qt::Key_Asterisk: return modifiers.testFlag(Qt::KeypadModifier) ? 0x6a : 0x38;
    case Qt::Key_ParenLeft: return 0x39;
    case Qt::Key_ParenRight: return 0x30;
    case Qt::Key_Plus: return modifiers.testFlag(Qt::KeypadModifier) ? 0x6b : 0xbb;
    case Qt::Key_Underscore:
    case Qt::Key_Minus: return 0xbd;
    case Qt::Key_Equal: return 0xbb;
    case Qt::Key_BraceLeft:
    case Qt::Key_BracketLeft: return 0xdb;
    case Qt::Key_BraceRight:
    case Qt::Key_BracketRight: return 0xdd;
    case Qt::Key_Bar:
    case Qt::Key_Backslash: return 0xdc;
    case Qt::Key_Colon:
    case Qt::Key_Semicolon: return 0xba;
    case Qt::Key_QuoteDbl:
    case Qt::Key_Apostrophe: return 0xde;
    case Qt::Key_AsciiTilde:
    case Qt::Key_QuoteLeft: return 0xc0;
    case Qt::Key_Less:
    case Qt::Key_Comma: return 0xbc;
    case Qt::Key_Greater:
    case Qt::Key_Period: return 0xbe;
    case Qt::Key_Question:
    case Qt::Key_Slash: return 0xbf;
    case Qt::Key_Right: return 0x27;
    case Qt::Key_Left: return 0x25;
    case Qt::Key_Down: return 0x28;
    case Qt::Key_Up: return 0x26;
    case Qt::Key_Control: return 0xa2;
    case Qt::Key_Shift: return 0xa0;
    case Qt::Key_Alt: return 0xa4;
    case Qt::Key_Meta: return 0x5b;
    case Qt::Key_CapsLock: return 0x14;
    case Qt::Key_NumLock: return 0x90;
    case Qt::Key_Insert: return 0x2d;
    case Qt::Key_Delete: return 0x2e;
    case Qt::Key_Home: return 0x24;
    case Qt::Key_End: return 0x23;
    case Qt::Key_PageUp: return 0x21;
    case Qt::Key_PageDown: return 0x22;
    case Qt::Key_Print: return 0x2a;
    case Qt::Key_ScrollLock: return 0x91;
    case Qt::Key_Pause: return 0x13;
    case Qt::Key_Menu: return 0x5d;
    default: return 0;
    }
}

quint16 StreamVideoItem::linuxPhysicalVirtualKey(quint32 nativeScanCode)
{
    switch (nativeScanCode) {
    case 49: return windowsVirtualKey(Qt::Key_QuoteLeft);

    case 10: return windowsVirtualKey(Qt::Key_1);
    case 11: return windowsVirtualKey(Qt::Key_2);
    case 12: return windowsVirtualKey(Qt::Key_3);
    case 13: return windowsVirtualKey(Qt::Key_4);
    case 14: return windowsVirtualKey(Qt::Key_5);
    case 15: return windowsVirtualKey(Qt::Key_6);
    case 16: return windowsVirtualKey(Qt::Key_7);
    case 17: return windowsVirtualKey(Qt::Key_8);
    case 18: return windowsVirtualKey(Qt::Key_9);
    case 19: return windowsVirtualKey(Qt::Key_0);
    case 20: return windowsVirtualKey(Qt::Key_Minus);
    case 21: return windowsVirtualKey(Qt::Key_Equal);

    case 24: return windowsVirtualKey(Qt::Key_Q);
    case 25: return windowsVirtualKey(Qt::Key_W);
    case 26: return windowsVirtualKey(Qt::Key_E);
    case 27: return windowsVirtualKey(Qt::Key_R);
    case 28: return windowsVirtualKey(Qt::Key_T);
    case 29: return windowsVirtualKey(Qt::Key_Y);
    case 30: return windowsVirtualKey(Qt::Key_U);
    case 31: return windowsVirtualKey(Qt::Key_I);
    case 32: return windowsVirtualKey(Qt::Key_O);
    case 33: return windowsVirtualKey(Qt::Key_P);
    case 34: return windowsVirtualKey(Qt::Key_BracketLeft);
    case 35: return windowsVirtualKey(Qt::Key_BracketRight);

    case 38: return windowsVirtualKey(Qt::Key_A);
    case 39: return windowsVirtualKey(Qt::Key_S);
    case 40: return windowsVirtualKey(Qt::Key_D);
    case 41: return windowsVirtualKey(Qt::Key_F);
    case 42: return windowsVirtualKey(Qt::Key_G);
    case 43: return windowsVirtualKey(Qt::Key_H);
    case 44: return windowsVirtualKey(Qt::Key_J);
    case 45: return windowsVirtualKey(Qt::Key_K);
    case 46: return windowsVirtualKey(Qt::Key_L);
    case 47: return windowsVirtualKey(Qt::Key_Semicolon);
    case 48: return windowsVirtualKey(Qt::Key_Apostrophe);
    case 51: return windowsVirtualKey(Qt::Key_Backslash);

    case 52: return windowsVirtualKey(Qt::Key_Z);
    case 53: return windowsVirtualKey(Qt::Key_X);
    case 54: return windowsVirtualKey(Qt::Key_C);
    case 55: return windowsVirtualKey(Qt::Key_V);
    case 56: return windowsVirtualKey(Qt::Key_B);
    case 57: return windowsVirtualKey(Qt::Key_N);
    case 58: return windowsVirtualKey(Qt::Key_M);
    case 59: return windowsVirtualKey(Qt::Key_Comma);
    case 60: return windowsVirtualKey(Qt::Key_Period);
    case 61: return windowsVirtualKey(Qt::Key_Slash);

    case 50: return 0xa0;
    case 62: return 0xa1;
    case 37: return 0xa2;
    case 105: return 0xa3;
    case 64: return 0xa4;
    case 108: return 0xa5;
    case 133: return 0x5b;
    case 134: return 0x5c;

    case 94: return 0xe2;

    default: return 0;
    }
}

quint16 StreamVideoItem::inputModifiers(Qt::KeyboardModifiers modifiers, int key)
{
    quint16 result = 0;
    if (key != Qt::Key_Shift && modifiers.testFlag(Qt::ShiftModifier)) result |= 0x01;
    if (key != Qt::Key_Control && modifiers.testFlag(Qt::ControlModifier)) result |= 0x02;
    if (key != Qt::Key_Alt && modifiers.testFlag(Qt::AltModifier)) result |= 0x04;
    if (key != Qt::Key_Meta && modifiers.testFlag(Qt::MetaModifier)) result |= 0x08;
    return result;
}

QString StreamVideoItem::shortcutActionForInput(
    const QVariantMap &bindings, int key, Qt::KeyboardModifiers modifiers)
{
    constexpr auto shortcutModifiers = Qt::ControlModifier | Qt::ShiftModifier
        | Qt::AltModifier | Qt::MetaModifier;
    const auto normalizedModifiers = modifiers & shortcutModifiers;
    for (auto binding = bindings.cbegin(); binding != bindings.cend(); ++binding) {
        QStringList sequences;
        if (binding.value().metaType().id() == QMetaType::QString) {
            sequences.push_back(binding.value().toString());
        } else {
            const auto values = binding.value().toList();
            sequences.reserve(values.size());
            for (const auto &value : values) sequences.push_back(value.toString());
        }
        for (const auto &text : std::as_const(sequences)) {
            const QKeySequence sequence(text, QKeySequence::PortableText);
            if (sequence.count() != 1) continue;
            const auto combination = sequence[0];
            if (combination.key() == static_cast<Qt::Key>(key)
                && (combination.keyboardModifiers() & shortcutModifiers)
                    == normalizedModifiers) {
                return binding.key();
            }
        }
    }
    return {};
}

void StreamVideoItem::focusInEvent(QFocusEvent *event)
{
    QQuickItem::focusInEvent(event);
    syncCaptureState();
}

void StreamVideoItem::focusOutEvent(QFocusEvent *event)
{
    releaseInput();
    syncCaptureState();
    QQuickItem::focusOutEvent(event);
}

quint16 StreamVideoItem::eventVirtualKey(const QKeyEvent *event)
{
#if defined(Q_OS_WIN)
    return windowsVirtualKey(event->key(), event->modifiers(), event->nativeVirtualKey());
#elif defined(Q_OS_LINUX)
    const auto physical = linuxPhysicalVirtualKey(event->nativeScanCode());
    if (physical != 0) return physical;
    return windowsVirtualKey(event->key(), event->modifiers());
#else
    return windowsVirtualKey(event->key(), event->modifiers());
#endif
}

quint32 StreamVideoItem::keyIdentity(const QKeyEvent *event) const
{
    if (event->nativeScanCode() != 0) return event->nativeScanCode();
    const auto virtualKey = eventVirtualKey(event);
    return virtualKey != 0 ? virtualKey : static_cast<quint32>(event->key());
}

void StreamVideoItem::keyPressEvent(QKeyEvent *event)
{
    const bool localRecovery = m_usesMacPointerCapture && m_relativeMouse && m_inputEnabled
        && isVisible() && hasActiveFocus() && window() && window()->isActive()
        && s_nativeRuntime && s_nativeRuntime->inputAllowed();
    if (!m_captureActive && !localRecovery) {
        event->ignore();
        return;
    }
    if (event->isAutoRepeat()) {
        event->accept();
        return;
    }
    const auto identity = keyIdentity(event);
    const auto virtualKey = eventVirtualKey(event);
    const QKeyEvent shortcutEvent(QEvent::KeyPress,
        virtualKey >= 0x41 && virtualKey <= 0x5a ? virtualKey : event->key(), event->modifiers());
    auto shortcutAction = shortcutActionForInput(
        m_shortcutBindings, event->key(), event->modifiers());
    if (shortcutAction.isEmpty() && shortcutEvent.key() != event->key()) {
        shortcutAction = shortcutActionForInput(
            m_shortcutBindings, shortcutEvent.key(), event->modifiers());
    }
    if (!shortcutAction.isEmpty()) {
        // A fullscreen transition can prevent Windows from delivering the key-up
        // that belongs to the key which initiated it.  Keep the identity only so
        // the eventual release is consumed; QKeyEvent::isAutoRepeat() already
        // prevents repeats, so a stale identity must never suppress the next
        // deliberate F11 press.
        m_pressedShortcuts.insert(identity);
        emit localShortcutRequested(shortcutAction);
        event->accept();
        return;
    }
    if (!m_captureActive) {
        event->ignore();
        return;
    }
    if (m_clipboardPaste && (event->matches(QKeySequence::Paste)
                            || shortcutEvent.matches(QKeySequence::Paste))) {
        m_pressedShortcuts.insert(identity);
        event->accept();
        const auto *clipboard = QGuiApplication::clipboard();
        const auto text = clipboard ? clipboard->text(QClipboard::Clipboard) : QString{};
        if (text.isEmpty() || text.size() > OPENNOW_STREAMER_MAX_TEXT_BYTES || !text.isValidUtf16()
            || text.contains(QChar::Null)) {
            emit clipboardPasteFailed();
            return;
        }
        const auto utf8 = text.toUtf8();
        if (utf8.size() > OPENNOW_STREAMER_MAX_TEXT_BYTES) {
            emit clipboardPasteFailed();
            return;
        }
        for (auto key = m_pressedKeys.begin(); key != m_pressedKeys.end();) {
            const auto vk = key->virtualKey;
            if ((vk >= 0xa0 && vk <= 0xa5) || vk == 0x5b || vk == 0x5c) {
                s_nativeRuntime->submitKey(vk, 0, false);
                key = m_pressedKeys.erase(key);
            } else {
                ++key;
            }
        }
        if (s_nativeRuntime->submitText(utf8) != OPENNOW_STREAMER_OK)
            emit clipboardPasteFailed();
        return;
    }
    if (virtualKey == 0) {
        event->ignore();
        return;
    }
    if (!m_pressedKeys.contains(identity)) {
        const auto modifiers = inputModifiers(event->modifiers(), event->key());
        m_pressedKeys.insert(identity, {virtualKey, modifiers});
        s_nativeRuntime->submitKey(virtualKey, modifiers, true);
    }
    event->accept();
}

void StreamVideoItem::keyReleaseEvent(QKeyEvent *event)
{
    if (event->isAutoRepeat()) {
        event->accept();
        return;
    }
    const auto identity = keyIdentity(event);
    if (m_pressedShortcuts.remove(identity)) {
        event->accept();
        return;
    }
    const auto pressed = m_pressedKeys.take(identity);
    if (pressed.virtualKey == 0) {
        event->ignore();
        return;
    }
    s_nativeRuntime->submitKey(pressed.virtualKey,
                             inputModifiers(event->modifiers(), event->key()), false);
    event->accept();
}

quint8 StreamVideoItem::mouseButton(Qt::MouseButton button)
{
    switch (button) {
    case Qt::LeftButton: return 1;
    case Qt::MiddleButton: return 2;
    case Qt::RightButton: return 3;
    case Qt::BackButton: return 4;
    case Qt::ForwardButton: return 5;
    default: return 0;
    }
}

void StreamVideoItem::mousePressEvent(QMouseEvent *event)
{
    const auto timingBegun = inputEventBegin();
    forceActiveFocus(Qt::MouseFocusReason);
    syncCaptureState();
    const auto button = mouseButton(event->button());
    if (m_captureActive && button != 0) {
        if (!m_pressedMouseButtons.contains(button)) {
            // In absolute cursor mode position and button must have one owner and
            // preserve their queue order.  A move event is not guaranteed before
            // a click (notably after a fullscreen viewport change).
            m_pressedMouseButtons.insert(button);
            if (!m_rawInputActive) {
                if (!m_relativeMouse) submitAbsoluteMouse(event->position());
                s_nativeRuntime->submitMouseButton(button, true);
                inputEventSubmitted(timingBegun);
            }
        }
        m_lastMousePosition = event->position();
        event->accept();
        return;
    }
    event->ignore();
}

void StreamVideoItem::mouseReleaseEvent(QMouseEvent *event)
{
    const auto timingBegun = inputEventBegin();
    const auto button = mouseButton(event->button());
    if (m_captureActive && button != 0) {
        if (m_pressedMouseButtons.remove(button) && !m_rawInputActive) {
            if (!m_relativeMouse) submitAbsoluteMouse(event->position());
            s_nativeRuntime->submitMouseButton(button, false);
            inputEventSubmitted(timingBegun);
        }
        if (m_pressedMouseButtons.isEmpty() && m_pendingRelativeMouse) {
            const auto relative = *m_pendingRelativeMouse;
            m_pendingRelativeMouse.reset();
            setRelativeMouse(relative);
        }
        event->accept();
        return;
    }
    event->ignore();
}

void StreamVideoItem::mouseMoveEvent(QMouseEvent *event)
{
    if (!m_captureActive) {
        event->ignore();
        return;
    }
    const auto timingBegun = inputEventBegin();
    if (m_relativeMouse) {
        if (!m_rawInputActive && !WaylandPointerCapture::isWayland()
                && !m_usesMacPointerCapture) {
            const auto delta = event->position() - m_lastMousePosition;
            const auto deltaX = std::clamp(qRound(delta.x()), -32768, 32767);
            const auto deltaY = std::clamp(qRound(delta.y()), -32768, 32767);
            if (deltaX != 0 || deltaY != 0)
                s_nativeRuntime->submitMouseRelative(static_cast<qint16>(deltaX),
                                                   static_cast<qint16>(deltaY));
            const auto anchor = mapToGlobal(QPointF(width() / 2.0, height() / 2.0)).toPoint();
            QCursor::setPos(anchor);
            m_lastMousePosition = mapFromGlobal(QCursor::pos());
            inputEventSubmitted(timingBegun);
        }
    } else {
        submitAbsoluteMouse(event->position());
        m_lastMousePosition = event->position();
        inputEventSubmitted(timingBegun);
    }
    event->accept();
}

void StreamVideoItem::hoverEnterEvent(QHoverEvent *event)
{
    if (!m_captureActive || m_relativeMouse) {
        event->ignore();
        return;
    }
    // QQuickItem sends ordinary no-button movement through hover events once
    // hover delivery is enabled. Publish the entry point as well so the remote
    // cursor cannot retain a stale position when it re-enters the stream item.
    const auto timingBegun = inputEventBegin();
    submitAbsoluteMouse(event->position());
    m_lastMousePosition = event->position();
    inputEventSubmitted(timingBegun);
    event->accept();
}

void StreamVideoItem::hoverMoveEvent(QHoverEvent *event)
{
    if (!m_captureActive || m_relativeMouse) {
        event->ignore();
        return;
    }
    // With no button held Qt does not call mouseMoveEvent for this item. Keep
    // the absolute GFN pointer current so remote hover and click hit-testing
    // use the same coordinates.
    const auto timingBegun = inputEventBegin();
    submitAbsoluteMouse(event->position());
    m_lastMousePosition = event->position();
    inputEventSubmitted(timingBegun);
    event->accept();
}

void StreamVideoItem::wheelEvent(QWheelEvent *event)
{
    if (!m_captureActive) {
        event->ignore();
        return;
    }
    const auto timingBegun = inputEventBegin();
    if (!m_rawInputActive) {
        if (!m_relativeMouse) submitAbsoluteMouse(event->position());
        const auto delta = event->pixelDelta().isNull() ? event->angleDelta()
                                                        : event->pixelDelta();
        s_nativeRuntime->submitMouseWheel(
            static_cast<qint16>(std::clamp(delta.x(), -32768, 32767)),
            static_cast<qint16>(std::clamp(delta.y(), -32768, 32767)));
        inputEventSubmitted(timingBegun);
    }
    event->accept();
}

void StreamVideoItem::itemChange(ItemChange change, const ItemChangeData &data)
{
    QQuickItem::itemChange(change, data);
    if (change == ItemVisibleHasChanged && !isVisible()) {
        m_manualRelativeMouse.reset();
        resetRemoteCursor();
    }
    if (change == ItemVisibleHasChanged || change == ItemSceneChange)
        syncCaptureState();
}

void StreamVideoItem::geometryChange(const QRectF &newGeometry, const QRectF &oldGeometry)
{
    QQuickItem::geometryChange(newGeometry, oldGeometry);
    resynchronizeInput();
}

QRect StreamVideoItem::scaledCaptureRect(const QRectF &itemRect,
                                         const QSizeF &windowSize,
                                         const QRect &clientScreenRect)
{
    if (!itemRect.isValid() || itemRect.isEmpty() || !windowSize.isValid()
        || windowSize.isEmpty() || !clientScreenRect.isValid()
        || clientScreenRect.isEmpty()) {
        return {};
    }
    const auto scaleX = clientScreenRect.width() / windowSize.width();
    const auto scaleY = clientScreenRect.height() / windowSize.height();
    const auto left = clientScreenRect.left()
        + static_cast<int>(std::floor(itemRect.left() * scaleX));
    const auto top = clientScreenRect.top()
        + static_cast<int>(std::floor(itemRect.top() * scaleY));
    const auto right = clientScreenRect.left()
        + static_cast<int>(std::ceil(itemRect.right() * scaleX));
    const auto bottom = clientScreenRect.top()
        + static_cast<int>(std::ceil(itemRect.bottom() * scaleY));
    return QRect(left, top, std::max(0, right - left),
                 std::max(0, bottom - top)).intersected(clientScreenRect);
}

QRect StreamVideoItem::absoluteMouseCoordinates(const QPointF &position,
                                                const QSize &videoSize,
                                                const QSizeF &itemSize)
{
    const auto target = QSize(std::max(1, qRound(itemSize.width())),
                              std::max(1, qRound(itemSize.height())));
    const auto viewport = aspectFitRect(videoSize, target);
    const auto x = std::clamp(qRound(position.x()) - viewport.x(), 0,
                              std::max(0, viewport.width() - 1));
    const auto y = std::clamp(qRound(position.y()) - viewport.y(), 0,
                              std::max(0, viewport.height() - 1));
    return QRect(x, y, viewport.width(), viewport.height());
}

StreamVideoItem::RemoteCursorMetadata StreamVideoItem::remoteCursorMetadata(
    const QByteArray &bytes)
{
    RemoteCursorMetadata result;
    if (bytes.size() < 7) return result;
    const auto messageType = static_cast<quint8>(bytes[0]);
    if (messageType > 1) return result;
    const auto mimeLength = static_cast<qsizetype>(static_cast<quint8>(bytes[4]));
    const auto lengthOffset = qsizetype{5} + mimeLength;
    if (lengthOffset < 5 || lengthOffset + 2 > bytes.size()) return result;
    const auto imageLength = static_cast<qsizetype>(
        static_cast<quint8>(bytes[lengthOffset])
        | (static_cast<quint16>(static_cast<quint8>(bytes[lengthOffset + 1])) << 8));
    const auto imageOffset = lengthOffset + 2;
    if (imageLength < 0 || imageOffset > bytes.size()
        || imageLength > bytes.size() - imageOffset) {
        return result;
    }
    result.imageOffset = imageOffset;
    result.imageLength = imageLength;
    const auto positionOffset = imageOffset + imageLength;
    if (positionOffset + 4 <= bytes.size()) {
        result.normalizedPosition = QPoint(
            static_cast<quint8>(bytes[positionOffset])
                | (static_cast<quint16>(static_cast<quint8>(bytes[positionOffset + 1])) << 8),
            static_cast<quint8>(bytes[positionOffset + 2])
                | (static_cast<quint16>(static_cast<quint8>(bytes[positionOffset + 3])) << 8));
    }
    const auto scaleOffset = positionOffset + 4;
    if (scaleOffset + 2 <= bytes.size()) {
        const auto scalePercent = static_cast<quint16>(
            static_cast<quint8>(bytes[scaleOffset])
            | (static_cast<quint16>(static_cast<quint8>(bytes[scaleOffset + 1])) << 8));
        if (scalePercent > 0) result.scale = scalePercent / 100.0;
    }
    return result;
}

QPoint StreamVideoItem::mapRemoteCursorPosition(const QPoint &normalizedPosition,
                                                const QSize &videoSize,
                                                const QSizeF &itemSize)
{
    const auto target = QSize(std::max(1, qRound(itemSize.width())),
                              std::max(1, qRound(itemSize.height())));
    const auto viewport = aspectFitRect(videoSize, target);
    const auto coordinate = [](int value, int extent) {
        const auto safeExtent = std::max(1, extent);
        return static_cast<int>(std::min<qint64>(
            (static_cast<qint64>(std::clamp(value, 0, 65535)) * safeExtent) / 65535,
            safeExtent - 1));
    };
    return QPoint(viewport.x() + coordinate(normalizedPosition.x(), viewport.width()),
                  viewport.y() + coordinate(normalizedPosition.y(), viewport.height()));
}

void StreamVideoItem::resynchronizeInput()
{
    syncCaptureState();
    if (m_captureActive && m_relativeMouse && !m_rawInputActive
            && !WaylandPointerCapture::isWayland() && !m_usesMacPointerCapture) {
        const auto anchor = mapToGlobal(QPointF(width() / 2.0, height() / 2.0)).toPoint();
        QCursor::setPos(anchor);
    } else if (m_captureActive && !m_relativeMouse) {
        // Fullscreen changes the absolute viewport dimensions without requiring
        // the physical cursor to move. Re-publish the current point immediately.
        submitAbsoluteMouse(mapFromGlobal(QCursor::pos()));
    }
    m_lastMousePosition = mapFromGlobal(QCursor::pos());
    updateCursorConfinement();
}

void StreamVideoItem::submitAbsoluteMouse(const QPointF &position)
{
    const auto coordinates = absoluteMouseCoordinates(
        position, m_videoSize, QSizeF(width(), height()));
    s_nativeRuntime->submitMouseAbsolute(
        static_cast<quint16>(std::min(coordinates.x(), 65535)),
        static_cast<quint16>(std::min(coordinates.y(), 65535)),
        static_cast<quint16>(std::min(coordinates.width(), 65535)),
        static_cast<quint16>(std::min(coordinates.height(), 65535)));
}

void StreamVideoItem::syncCaptureState()
{
    auto desired = m_inputEnabled && isVisible() && hasActiveFocus()
        && window() && window()->isActive() && s_nativeRuntime && s_nativeRuntime->running()
        && s_nativeRuntime->inputAllowed();
    if (m_captureActive && (!desired || (WaylandPointerCapture::isWayland()
            && m_relativeMouse && !m_waylandPointer->locked())
            || (m_usesMacPointerCapture && m_relativeMouse && !m_macPointer->locked())))
        releaseInput();
    if (WaylandPointerCapture::isWayland()) {
        const auto viewport = aspectFitRect(m_videoSize, QSize(qRound(width()), qRound(height())));
        auto region = mapRectToScene(QRectF(viewport)).toAlignedRect();
        if (window()) region.translate(window()->frameMargins().left(), window()->frameMargins().top());
        m_waylandPointer->setCapture(window(), desired && m_relativeMouse, region);
        if (m_relativeMouse) desired = desired && m_waylandPointer->locked();
    }
    if (m_usesMacPointerCapture) {
        const auto viewport = aspectFitRect(m_videoSize, QSize(qRound(width()), qRound(height())));
        m_macPointer->setCapture(window(), desired && m_relativeMouse,
                                 mapRectToScene(QRectF(viewport)).toAlignedRect());
        if (m_relativeMouse) desired = desired && m_macPointer->locked();
    }
    bool rawInput = false;
    if (s_nativeRuntime && s_nativeRuntime->running()) {
        s_nativeRuntime->setCaptureActive(
            desired, m_relativeMouse,
            window() && !WaylandPointerCapture::isWayland()
#if defined(Q_OS_LINUX)
                && QGuiApplication::platformName() == QStringLiteral("xcb")
#endif
                ? static_cast<std::uintptr_t>(window()->winId()) : 0,
            &rawInput);
    }
    m_rawInputActive = desired && rawInput;
    const auto changed = m_captureActive != desired;
    m_captureActive = desired;
    if (m_captureActive) {
        m_lastMousePosition = mapFromGlobal(QCursor::pos());
        if (m_relativeMouse && !WaylandPointerCapture::isWayland()
                && !m_usesMacPointerCapture) grabMouse();
    } else {
        ungrabMouse();
    }
    updateCursorConfinement();
    updateLocalCursor();
    if (changed) emit captureActiveChanged();
}

void StreamVideoItem::releaseInput()
{
    m_waylandPointer->release();
    m_macPointer->release();
    const auto pendingRelativeMouse = m_pendingRelativeMouse;
    m_pendingRelativeMouse.reset();
    if (s_nativeRuntime) {
        for (const auto &pressed : std::as_const(m_pressedKeys))
            s_nativeRuntime->submitKey(pressed.virtualKey, 0, false);
    }
    m_pressedKeys.clear();
    m_pressedShortcuts.clear();
    if (!m_rawInputActive) releaseQtMouseButtons();
    else m_pressedMouseButtons.clear();
    ungrabMouse();
    releaseCursorConfinement();
    // Remember the newest server mode across focus loss without re-entering
    // syncCaptureState() or acquiring a new grab while releasing the old one.
    if (pendingRelativeMouse && m_relativeMouse != *pendingRelativeMouse) {
        m_relativeMouse = *pendingRelativeMouse;
        emit relativeMouseChanged();
    }
    unsetCursor();
}

void StreamVideoItem::releaseQtMouseButtons()
{
    if (s_nativeRuntime) {
        for (const auto button : std::as_const(m_pressedMouseButtons))
            s_nativeRuntime->submitMouseButton(button, false);
    }
    m_pressedMouseButtons.clear();
}

QRect StreamVideoItem::cursorConfinementRect(const QRect &viewport, bool rawRelative)
{
    // Raw Input supplies unaccelerated deltas independently of the OS cursor.
    // Pin that hidden cursor so Qt cannot hover chrome as the player looks around.
    // The non-raw relative fallback still needs room for its move/recenter events.
    return rawRelative && !viewport.isEmpty() ? QRect(viewport.center(), QSize(1, 1)) : viewport;
}

void StreamVideoItem::updateCursorConfinement()
{
#if defined(Q_OS_WIN)
    if (!m_captureActive || !window() || !window()->isActive()) {
        releaseCursorConfinement();
        return;
    }
    const auto handle = reinterpret_cast<HWND>(window()->winId());
    RECT client{};
    POINT topLeft{};
    if (!handle || !GetClientRect(handle, &client)
        || !ClientToScreen(handle, &topLeft)) {
        releaseCursorConfinement();
        return;
    }
    POINT bottomRight{client.right, client.bottom};
    if (!ClientToScreen(handle, &bottomRight)) {
        releaseCursorConfinement();
        return;
    }
    const auto *content = window()->contentItem();
    if (!content) {
        releaseCursorConfinement();
        return;
    }
    const auto first = mapToItem(content, QPointF(0, 0));
    const auto second = mapToItem(content, QPointF(width(), height()));
    const QRectF itemRect(QPointF(std::min(first.x(), second.x()),
                                 std::min(first.y(), second.y())),
                          QPointF(std::max(first.x(), second.x()),
                                  std::max(first.y(), second.y())));
    const auto captureRect = scaledCaptureRect(
        itemRect, QSizeF(window()->width(), window()->height()),
        QRect(topLeft.x, topLeft.y, bottomRight.x - topLeft.x,
              bottomRight.y - topLeft.y));
    if (captureRect.isEmpty()) {
        releaseCursorConfinement();
        return;
    }
    const auto confinement = cursorConfinementRect(captureRect, m_relativeMouse && m_rawInputActive);
    const RECT screenRect{confinement.left(), confinement.top(),
                          confinement.left() + confinement.width(),
                          confinement.top() + confinement.height()};
    m_cursorConfined = ClipCursor(&screenRect) != FALSE;
#else
    m_cursorConfined = false;
#endif
}

void StreamVideoItem::releaseCursorConfinement()
{
#if defined(Q_OS_WIN)
    if (m_cursorConfined) ClipCursor(nullptr);
#endif
    m_cursorConfined = false;
}

void StreamVideoItem::setRelativeMouse(bool relative)
{
    // Server cursor visibility can change during remote window dragging. Keep
    // the button owner and pointer coordinates stable until the physical release.
    // Focus loss/overlays still release everything through releaseInput().
    if (!m_pressedMouseButtons.isEmpty()) {
        m_pendingRelativeMouse = relative;
        return;
    }
    m_pendingRelativeMouse.reset();
    if (m_relativeMouse == relative) return;
    // Qt owns buttons while the remote cursor is visible; Windows Raw Input
    // owns them in relative mode. A raw release cannot match a button that Qt
    // pressed, so close the old ownership epoch before enabling Raw Input.
    if (relative && !m_rawInputActive) releaseQtMouseButtons();
    m_relativeMouse = relative;
    if (relative) {
        if (m_captureActive && !WaylandPointerCapture::isWayland()
                && !m_usesMacPointerCapture) {
            grabMouse();
            const auto anchor = mapToGlobal(QPointF(width() / 2.0, height() / 2.0)).toPoint();
            QCursor::setPos(anchor);
            m_lastMousePosition = mapFromGlobal(QCursor::pos());
        }
    } else {
        ungrabMouse();
        releaseCursorConfinement();
    }
    syncCaptureState();
    emit relativeMouseChanged();
}

void StreamVideoItem::setRemoteCursorShape(const QCursor &cursor)
{
    m_remoteCursor = cursor;
    updateLocalCursor();
}

void StreamVideoItem::updateLocalCursor()
{
    if (!m_captureActive) {
        unsetCursor();
        return;
    }
    // Known-hidden (the seat reported id 0) blanks the pointer as well: with the
    // seat silent about a shape, an arrow would float over the game's own
    // cursor. An *unknown* visibility still shows the last shape, because hiding
    // on "nothing published" leaves a session with no pointer at all.
    if (m_relativeMouse
        || (m_serverCursorComposited && m_manualRelativeMouse != false)
        || (m_remoteCursorKnown && !m_remoteCursorVisible)) {
        setCursor(Qt::BlankCursor);
        return;
    }
    setCursor(m_remoteCursor);
}

namespace {
/// Optional dev override for the absolute-on-hidden cursor policy:
/// `OPENNOW_CURSOR_ABSOLUTE_HIDDEN=0` forces the old lock-on-hidden behaviour
/// regardless of the setting. Empty means "use the setting".
std::optional<bool> cursorAbsoluteOnHiddenOverride()
{
    const auto value = qEnvironmentVariable("OPENNOW_CURSOR_ABSOLUTE_HIDDEN");
    if (value.isEmpty()) return std::nullopt;
    return value != QStringLiteral("0")
        && value.compare(QStringLiteral("false"), Qt::CaseInsensitive) != 0;
}
} // namespace

bool StreamVideoItem::relativeInputForRemoteCursor(bool hidden,
                                                   bool currentRelative,
                                                   std::optional<bool> manualRelative,
                                                   bool absoluteOnHidden)
{
    if (manualRelative) return *manualRelative;
    if (!absoluteOnHidden) return hidden;
    // Stay put while the seat reports hidden: this policy must not *enter*
    // relative mode, and it must not kick an already-locked input out of it.
    // A visible cursor still returns to absolute, as before.
    return hidden ? currentRelative : false;
}

void StreamVideoItem::resetRemoteCursor()
{
    m_remoteCursorKnown = false;
    m_remoteCursorVisible = false;
    m_serverCursorComposited = s_nativeRuntime ? s_nativeRuntime->serverCursorComposited() : true;
    m_remoteCursor = QCursor();
    setRelativeMouse(m_manualRelativeMouse.value_or(false));
    updateLocalCursor();
}

void StreamVideoItem::togglePointerLock()
{
    m_manualRelativeMouse = !m_relativeMouse;
    releaseInput();
    setRelativeMouse(*m_manualRelativeMouse);
    syncCaptureState();
}

void StreamVideoItem::applyRemoteCursor(const QByteArray &bytes)
{
    if (bytes.size() < 2) return;
    const auto messageType = static_cast<quint8>(bytes[0]);
    const auto cursorId = static_cast<quint8>(bytes[1]);
    if (messageType > 1) return;
    const auto hidden = messageType == 0 && cursorId == 0;
    const auto reposition = !m_remoteCursorKnown || !m_remoteCursorVisible;
    const auto metadata = remoteCursorMetadata(bytes);
    m_remoteCursorKnown = true;
    m_remoteCursorVisible = !hidden;
    // Settings-driven absolute-on-hidden policy (default on — the official
    // client stayed absolute through the recorded Path of Exile 2 session,
    // `Local cursor: 1 … shouldLock: 0` in geronimo.log.bak, and the
    // OpenNOW-Mac reference keys input mode on the player's pointer lock,
    // not on the seat's visibility). Switching to relative hides the host
    // cursor, which is what stops the seat publishing a shape at all,
    // leaving only the game-drawn cursor in the video. The env variable
    // remains a dev override.
    setRelativeMouse(relativeInputForRemoteCursor(
        hidden, m_relativeMouse, m_manualRelativeMouse,
        cursorAbsoluteOnHiddenOverride().value_or(m_absoluteCursorOnHidden)));
    updateLocalCursor();
    if (hidden) return;

    if (reposition && metadata.normalizedPosition && m_pressedMouseButtons.isEmpty()
        && m_captureActive && !m_relativeMouse && !WaylandPointerCapture::isWayland()) {
        const auto local = mapRemoteCursorPosition(
            *metadata.normalizedPosition, m_videoSize, QSizeF(width(), height()));
        QCursor::setPos(mapToGlobal(local).toPoint());
        m_lastMousePosition = local;
    }

    if (messageType == 1 && metadata.imageOffset >= 0 && metadata.imageLength > 0) {
        QPixmap pixmap;
        const auto image = QByteArray::fromBase64(
            bytes.mid(metadata.imageOffset, metadata.imageLength));
        if (pixmap.loadFromData(image) && pixmap.width() <= 256 && pixmap.height() <= 256) {
            const auto scaledSize = QSize(
                std::clamp(qRound(pixmap.width() / metadata.scale), 1, 256),
                std::clamp(qRound(pixmap.height() / metadata.scale), 1, 256));
            if (scaledSize != pixmap.size())
                pixmap = pixmap.scaled(scaledSize, Qt::IgnoreAspectRatio,
                                       Qt::SmoothTransformation);
            const auto hotspot = QPoint(
                std::clamp(qRound(static_cast<quint8>(bytes[2]) / metadata.scale),
                           0, pixmap.width() - 1),
                std::clamp(qRound(static_cast<quint8>(bytes[3]) / metadata.scale),
                           0, pixmap.height() - 1));
            setRemoteCursorShape(QCursor(pixmap, hotspot.x(), hotspot.y()));
            return;
        }
    }
    switch (cursorId) {
    case 2: setRemoteCursorShape(Qt::IBeamCursor); break;
    case 3: setRemoteCursorShape(Qt::WaitCursor); break;
    case 4: setRemoteCursorShape(Qt::CrossCursor); break;
    case 6: setRemoteCursorShape(Qt::SizeFDiagCursor); break;
    case 7: setRemoteCursorShape(Qt::SizeBDiagCursor); break;
    case 8: setRemoteCursorShape(Qt::SizeHorCursor); break;
    case 9: setRemoteCursorShape(Qt::SizeVerCursor); break;
    case 10: setRemoteCursorShape(Qt::SizeAllCursor); break;
    case 12: setRemoteCursorShape(Qt::PointingHandCursor); break;
    default: setRemoteCursorShape(Qt::ArrowCursor); break;
    }
}
