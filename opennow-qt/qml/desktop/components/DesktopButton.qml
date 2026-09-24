import QtQuick
import QtQuick.Controls
import OpenNOW

Button {
    id: root
    property bool primary: false
    property bool danger: false
    property bool onMediaBackground: false
    // Borderless text/icon action that only shows a fill on hover or focus.
    property bool quiet: false
    // Checked state reads as an accent-tinted outline instead of a white slab.
    property bool tinted: false
    readonly property bool tintedOn: tinted && checked
    property string shortcutText: ""
    property string shortcutSequence: shortcutText
    property string glyph: ""
    property string themedGlyph: ""
    property int glyphSize: 16
    property int cornerRadius: 10
    height: 36
    implicitWidth: Math.max(68, contentRow.implicitWidth + leftPadding + rightPadding)
    leftPadding: 14
    rightPadding: 14
    focusPolicy: Qt.StrongFocus
    font.family: DesktopTokens.bodyFont
    font.pixelSize: 12
    font.weight: Font.ExtraBold
    background: Rectangle {
        radius: root.cornerRadius
        color: root.tintedOn ? Qt.rgba(Theme.focus.r, Theme.focus.g, Theme.focus.b, root.down ? 0.26 : 0.16)
             : root.primary ? (root.down ? "#D9D9D9" : "#FFFFFFFF")
             : root.danger ? (root.hovered || root.activeFocus ? "#29FF8A80" : "#14FF8A80")
             : root.quiet ? (root.down ? DesktopTokens.raisedStrong : root.hovered || root.activeFocus ? DesktopTokens.raised : "transparent")
             : (root.hovered || root.activeFocus ? "#1FFFFFFF" : "#0FFFFFFF")
        border.width: root.tintedOn ? 1 : root.primary || root.quiet ? 0 : 1
        border.color: root.tintedOn ? Theme.focus : root.danger ? "#52FF8A80" : "#1FFFFFFF"
        scale: root.down && !AppController.reducedMotion ? 0.985 : 1
        Behavior on color { ColorAnimation { duration: DesktopTokens.quickDuration } }
        Behavior on scale { NumberAnimation { duration: DesktopTokens.quickDuration; easing.type: Easing.OutCubic } }
        Rectangle {
            anchors.fill: parent
            anchors.margins: -2
            radius: parent.radius + 2
            color: "transparent"
            border.width: 2
            border.color: root.onMediaBackground ? Theme.mediaAccent : DesktopTokens.focus
            visible: root.activeFocus
        }
    }
    contentItem: Item {
        implicitWidth: contentRow.implicitWidth
        implicitHeight: contentRow.implicitHeight
        Row {
            id: contentRow
            anchors.centerIn: parent
            spacing: root.glyph !== "" || root.themedGlyph !== "" ? 11 : 8
            DesktopGlyph {
                visible: root.glyph !== "" && root.themedGlyph === ""
                anchors.verticalCenter: parent.verticalCenter
                width: root.glyphSize
                height: root.glyphSize
                icon: root.glyph
            }
            Loader {
                active: root.themedGlyph !== ""
                visible: active
                anchors.verticalCenter: parent.verticalCenter
                width: root.glyphSize; height: root.glyphSize
                sourceComponent: DesktopSettingsIcon {
                    glyph: root.themedGlyph
                    ink: root.primary ? "#0A0D14" : root.onMediaBackground ? Theme.mediaForeground : Theme.label
                }
            }
            Text {
                visible: root.text !== ""
                anchors.verticalCenter: parent.verticalCenter
                text: root.text
                color: root.primary ? "#0A0D14" : root.danger ? "#FFB4AE"
                    : root.onMediaBackground ? Theme.mediaForeground
                    : root.quiet && !root.hovered && !root.activeFocus ? DesktopTokens.textMuted : DesktopTokens.textHigh
                font: root.font
            }
            KeyboardGlyph {
                visible: root.shortcutText !== ""
                anchors.verticalCenter: parent.verticalCenter
                shortcut: root.shortcutSequence
                Accessible.name: root.shortcutText
                keySize: 20
                ink: root.primary ? "#0B0F1A" : root.onMediaBackground ? Theme.mediaMuted : DesktopTokens.textMuted
            }
        }
    }
}
