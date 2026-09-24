import QtQuick
import QtQuick.Shapes
import OpenNOW

Column {
    id: root
    required property var game
    // Variant: accent play control that matches the game details modal.
    // The default keeps the original white bar for any other caller.
    property bool refined: false
    spacing: refined ? 9 : 7

    Text {
        width: parent.width
        text: root.game ? String(root.game.title || qsTr("Game")) : qsTr("Game")
        color: Theme.mediaForeground
        elide: Text.ElideRight
        font.family: DesktopTokens.bodyFont
        font.pixelSize: root.refined ? 13 : 12
        font.weight: root.refined ? Font.Black : Font.Bold
        font.letterSpacing: root.refined ? -0.1 : 0
    }

    Rectangle {
        id: playControl
        readonly property color ink: Theme.focusText
        visible: root.refined
        width: parent.width
        height: 34
        radius: 12
        color: Theme.focus
        Rectangle {
            anchors.fill: parent
            anchors.margins: 1
            radius: parent.radius - 1
            color: "transparent"
            border.width: 1
            border.color: "#38FFFFFF"
        }
        Row {
            anchors.verticalCenter: parent.verticalCenter
            x: 6
            spacing: 8
            Item {
                width: 22
                height: 22
                Rectangle {
                    anchors.fill: parent
                    radius: width / 2
                    color: Qt.rgba(playControl.ink.r, playControl.ink.g, playControl.ink.b, 0.14)
                }
                // Nudged right of centre: a centred triangle reads left-heavy.
                Shape {
                    anchors.centerIn: parent
                    anchors.horizontalCenterOffset: 1
                    width: 8
                    height: 10
                    layer.enabled: true
                    layer.samples: 4
                    ShapePath {
                        fillColor: playControl.ink
                        strokeColor: playControl.ink
                        strokeWidth: 1.5
                        joinStyle: ShapePath.RoundJoin
                        startX: 0.75
                        startY: 0.75
                        PathLine { x: 7.25; y: 5 }
                        PathLine { x: 0.75; y: 9.25 }
                        PathLine { x: 0.75; y: 0.75 }
                    }
                }
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: qsTr("Play")
                color: playControl.ink
                font.family: DesktopTokens.bodyFont
                font.pixelSize: 13
                font.weight: Font.Black
            }
        }
    }

    Rectangle {
        visible: !root.refined
        width: parent.width
        height: 32
        radius: 8
        color: "#F2FFFFFF"

        Row {
            anchors.centerIn: parent
            spacing: 6

            DesktopGlyph {
                anchors.verticalCenter: parent.verticalCenter
                width: 18
                height: 18
                icon: "desktop-play-filled.svg"
                sourceSize: Qt.size(Math.ceil(width * dpr * DesktopTokens.cardHoverScale),
                                    Math.ceil(height * dpr * DesktopTokens.cardHoverScale))
                smooth: true
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: qsTr("Play")
                color: "#0B0F1A"
                font.family: DesktopTokens.bodyFont
                font.pixelSize: 12
                font.weight: Font.Bold
            }
        }
    }
}
