import QtQuick
import QtQuick.Controls
import QtQuick.Effects
import QtQuick.Shapes
import OpenNOW

Item {
    id: root

    // Marquee slides from the CMS panels document:
    // {kind:"game"|"marketing", title, body, image, game?, actionLabel?}
    property var slides: []
    property int currentSlide: 0
    property int selectedAction: -1

    signal playRequested(var game)
    signal detailsRequested(var game)
    signal actionPointed(int index)

    width: 1160
    height: 260

    readonly property var slide: slides.length ? slides[Math.max(0, Math.min(currentSlide, slides.length - 1))] : null
    readonly property var slideGame: slide && slide.game ? slide.game : null

    function nextSlide() {
        if (slides.length > 1)
            currentSlide = (currentSlide + 1) % slides.length
    }

    onSlidesChanged: currentSlide = 0

    Timer {
        id: advanceTimer
        interval: 6000
        repeat: true
        running: root.visible && slides.length > 1 && !AppController.reducedMotion && !heroHover.hovered
        onTriggered: root.nextSlide()
    }

    // Keep the text's contrast backing outside the effect layer. Qt's software
    // renderer cannot draw MultiEffect masks, but must still show a readable hero.
    Rectangle {
        anchors.fill: parent
        radius: 16
        color: "#0B0F1A"
    }

    Rectangle {
        id: heroMask
        anchors.fill: parent
        radius: 16
        color: "white"
        visible: false
        layer.enabled: true
    }

    Item {
        anchors.fill: parent
        layer.enabled: true
        layer.smooth: true
        layer.effect: MultiEffect {
            maskEnabled: true
            maskSource: heroMask
            maskThresholdMin: 0.25
            maskSpreadAtMin: 0.2
        }

        Repeater {
            model: root.slides
            Item {
                required property var modelData
                required property int index
                anchors.fill: parent
                visible: opacity > 0
                opacity: index === root.currentSlide ? 1 : 0
                Behavior on opacity { NumberAnimation { duration: AppController.reducedMotion ? 0 : 450; easing.type: Easing.OutCubic } }

                Image {
                    x: Math.round(parent.width * 0.28)
                    width: parent.width - x
                    height: parent.height
                    source: DesktopTokens.decodeArtworkUrl(String(modelData.image || ""))
                    fillMode: Image.PreserveAspectCrop
                    asynchronous: true
                    cache: true
                    sourceSize: Qt.size(Math.ceil(width), Math.ceil(height))
                }
            }
        }

        Rectangle {
            anchors.fill: parent
            gradient: Gradient {
                orientation: Gradient.Horizontal
                GradientStop { position: 0; color: Qt.rgba(0.043, 0.059, 0.102, 0.96) }
                GradientStop { position: 0.45; color: Qt.rgba(0.043, 0.059, 0.102, 0.72) }
                GradientStop { position: 0.75; color: Qt.rgba(0.043, 0.059, 0.102, 0.12) }
                GradientStop { position: 1; color: "transparent" }
            }
        }
    }

    Rectangle {
        anchors.fill: parent
        radius: 16
        color: "transparent"
        border.width: 1
        border.color: Qt.rgba(1, 1, 1, 0.12)
    }

    HoverHandler { id: heroHover }

    Column {
        x: 24
        y: 24
        width: Math.min(520, Math.max(300, root.width * 0.42))
        spacing: 10

        Text {
            text: root.slide && root.slide.kind === "marketing" ? qsTr("GEFORCE NOW") : qsTr("FEATURED")
            color: DesktopTokens.focus
            font.family: Theme.monoFont
            font.pixelSize: DesktopTokens.microSize
            font.weight: Font.Bold
            font.letterSpacing: 1.1
        }

        Text {
            width: parent.width
            text: root.slide ? String(root.slide.title || "") : ""
            color: "#FFFFFF"
            font.family: Theme.displayFont
            font.pixelSize: DesktopTokens.px(34)
            font.weight: Font.Black
            font.letterSpacing: -1.1
            elide: Text.ElideRight
            maximumLineCount: 2
            wrapMode: Text.WordWrap
        }

        Text {
            width: parent.width
            visible: text !== ""
            text: root.slide ? String(root.slide.body || "") : ""
            color: Qt.rgba(1, 1, 1, 0.72)
            font.family: Theme.bodyFont
            font.pixelSize: DesktopTokens.captionSize
            font.weight: Font.Medium
            lineHeightMode: Text.FixedHeight
            lineHeight: 19
            maximumLineCount: 2
            elide: Text.ElideRight
            wrapMode: Text.WordWrap
        }
    }

    Row {
        x: 24
        y: parent.height - 68
        height: 44
        spacing: 10
        visible: root.slideGame !== null

        Button {
            id: playButton
            readonly property color ink: Theme.focusText
            width: 128
            height: 44
            padding: 0
            focusPolicy: Qt.NoFocus
            hoverEnabled: true
            Accessible.name: text
            text: qsTr("Play")
            onHoveredChanged: if (hovered) root.actionPointed(0)
            onClicked: if (root.slideGame) root.playRequested(root.slideGame)
            background: Rectangle {
                radius: 14
                color: Theme.focus
                scale: playButton.down && !AppController.reducedMotion ? 0.98 : 1
                Behavior on scale { NumberAnimation { duration: DesktopTokens.quickDuration; easing.type: Easing.OutCubic } }
                Rectangle {
                    anchors.fill: parent
                    radius: parent.radius
                    color: "#FFFFFF"
                    opacity: playButton.hovered ? 0.16 : 0
                    Behavior on opacity { NumberAnimation { duration: DesktopTokens.quickDuration } }
                }
                Rectangle {
                    anchors.fill: parent
                    anchors.margins: 1
                    radius: parent.radius - 1
                    color: "transparent"
                    border.width: 1
                    border.color: "#38FFFFFF"
                }
                Rectangle {
                    anchors.fill: parent
                    anchors.margins: -3
                    radius: parent.radius + 3
                    color: "transparent"
                    border.width: 2
                    border.color: "#FFFFFF"
                    visible: root.selectedAction === 0
                }
            }
            contentItem: Row {
                spacing: 10
                anchors.centerIn: parent
                Item {
                    width: 26
                    height: 26
                    anchors.verticalCenter: parent.verticalCenter
                    Rectangle {
                        anchors.fill: parent
                        radius: width / 2
                        color: Qt.rgba(playButton.ink.r, playButton.ink.g, playButton.ink.b, 0.14)
                    }
                    // Nudged right of centre: a centred triangle reads left-heavy.
                    Shape {
                        anchors.centerIn: parent
                        anchors.horizontalCenterOffset: 1
                        width: 9
                        height: 11
                        layer.enabled: true
                        layer.samples: 4
                        ShapePath {
                            fillColor: playButton.ink
                            strokeColor: playButton.ink
                            strokeWidth: 1.5
                            joinStyle: ShapePath.RoundJoin
                            startX: 0.75
                            startY: 0.75
                            PathLine { x: 8.25; y: 5.5 }
                            PathLine { x: 0.75; y: 10.25 }
                            PathLine { x: 0.75; y: 0.75 }
                        }
                    }
                }
                Text {
                    anchors.verticalCenter: parent.verticalCenter
                    text: playButton.text
                    color: playButton.ink
                    font.family: Theme.displayFont
                    font.pixelSize: DesktopTokens.bodySize
                    font.weight: Font.Black
                }
            }
        }

        Button {
            id: detailsButton
            anchors.verticalCenter: parent.verticalCenter
            width: 132
            height: 40
            padding: 0
            focusPolicy: Qt.NoFocus
            hoverEnabled: true
            Accessible.name: text
            text: qsTr("View details")
            onHoveredChanged: if (hovered) root.actionPointed(1)
            onClicked: if (root.slideGame) root.detailsRequested(root.slideGame)
            background: Rectangle {
                radius: 12
                color: detailsButton.down ? Qt.rgba(1, 1, 1, 0.16) : detailsButton.hovered ? Qt.rgba(1, 1, 1, 0.12) : Qt.rgba(1, 1, 1, 0.06)
                border.width: root.selectedAction === 1 ? 2 : 0
                border.color: "#FFFFFF"
                Behavior on color { ColorAnimation { duration: DesktopTokens.quickDuration } }
            }
            contentItem: Text {
                text: detailsButton.text
                color: Qt.rgba(1, 1, 1, 0.88)
                font.family: Theme.bodyFont
                font.pixelSize: DesktopTokens.captionSize
                font.weight: Font.Bold
                horizontalAlignment: Text.AlignHCenter
                verticalAlignment: Text.AlignVCenter
            }
        }
    }

    Row {
        x: 24
        y: parent.height - 20
        spacing: 6
        visible: root.slides.length > 1
        Repeater {
            model: root.slides.length
            Rectangle {
                required property int index
                width: index === root.currentSlide ? 18 : 6
                height: 6
                radius: 3
                color: index === root.currentSlide ? DesktopTokens.focus : Qt.rgba(1, 1, 1, 0.28)
                Behavior on width { NumberAnimation { duration: AppController.reducedMotion ? 0 : 180; easing.type: Easing.OutCubic } }
                HoverHandler { cursorShape: Qt.PointingHandCursor }
                TapHandler { onTapped: root.currentSlide = index }
            }
        }
    }
}
