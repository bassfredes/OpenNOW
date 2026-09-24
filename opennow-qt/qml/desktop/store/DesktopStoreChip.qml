import QtQuick
import QtQuick.Controls
import OpenNOW

Button {
    id: root

    property bool selected: false
    property bool hasMenu: false

    implicitWidth: contentRow.implicitWidth + DesktopTokens.px(26)
    implicitHeight: DesktopTokens.px(30)
    padding: 0
    focusPolicy: Qt.NoFocus
    hoverEnabled: true
    Accessible.role: Accessible.Button
    Accessible.name: text

    background: Rectangle {
        radius: DesktopTokens.px(9)
        color: root.selected ? Qt.rgba(DesktopTokens.focus.r, DesktopTokens.focus.g, DesktopTokens.focus.b, root.down ? 0.26 : 0.16)
             : root.down ? DesktopTokens.raisedStrong
             : root.hovered ? DesktopTokens.raised : DesktopTokens.seamSoft
        border.width: root.selected ? 1 : 0
        border.color: DesktopTokens.focus

        Behavior on color {
            ColorAnimation { duration: Theme.focusDuration }
        }
        Behavior on border.color {
            ColorAnimation { duration: Theme.focusDuration }
        }
    }

    contentItem: Item {
      Row {
        id: contentRow
        anchors.centerIn: parent
        spacing: root.hasMenu ? DesktopTokens.px(7) : 0

        Text {
            anchors.verticalCenter: parent.verticalCenter
            text: root.text
            color: root.selected ? DesktopTokens.text : DesktopTokens.textMuted
            font.family: Theme.bodyFont
            font.pixelSize: DesktopTokens.monoSize
            font.weight: root.selected ? Font.ExtraBold : Font.DemiBold
        }

        DesktopSettingsIcon {
            anchors.verticalCenter: parent.verticalCenter
            visible: root.hasMenu
            width: DesktopTokens.px(10)
            height: width
            glyph: "chevron"
            rotation: 90
            ink: DesktopTokens.textMuted
        }
    }
    }
}
