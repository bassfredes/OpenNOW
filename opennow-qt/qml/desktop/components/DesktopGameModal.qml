import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Shapes
import QtQuick.Window
import OpenNOW

FocusScope {
    id: root
    property var game: ShellStore.selectedGame
    signal closeRequested()
    signal playRequested()
    signal variantSelected(int index)
    objectName: "desktopGameModal"
    property bool opened: false
    visible: reveal.present
    enabled: opened
    focus: opened
    onOpenedChanged: if (opened) Qt.callLater(() => { if (root.opened) primaryAction.forceActiveFocus() })
    MotionProgress { id: reveal; objectName: "gameDetailsMotion"; shown: root.opened }

    readonly property int gutter: DesktopTokens.px(32)
    readonly property int dialogWidth: Math.min(DesktopTokens.px(760), Math.max(0, width - gutter * 2))
    readonly property int maximumDialogHeight: Math.max(0, height - gutter * 2)
    readonly property int dialogHeight: Math.min(maximumDialogHeight, Math.ceil(detailsColumn.implicitHeight))
    readonly property var streamSettings: ShellStore.settings || ({})
    readonly property var selectedVariant: {
        const game = root.game
        if (!game)
            return null
        const variants = game.variants || []
        if (!variants.length)
            return null
        const index = Number(game.selectedVariantIndex || 0)
        return index >= 0 && index < variants.length ? variants[index] : null
    }
    readonly property bool gameAvailable: {
        const game = root.game
        if (!game)
            return false
        return ShellStore.selectedLaunchDecision.status === "ready"
    }
    readonly property string lastPlayedText: {
        const game = root.game
        if (!game)
            return qsTr("—")
        if (game.lastPlayedLabel)
            return String(game.lastPlayedLabel)
        const raw = String(game.lastPlayed || (root.selectedVariant && root.selectedVariant.lastPlayedDate) || "")
        if (!raw)
            return qsTr("Not played yet")
        return DesktopTokens.relativeLastPlayed(raw, Date.now()) || qsTr("—")
    }
    readonly property string storesText: {
        const game = root.game
        if (!game)
            return qsTr("—")
        const fromStores = game.availableStores || []
        const stores = fromStores.length
            ? fromStores
            : (game.variants || []).map(variant => variant && variant.store).filter(Boolean)
        return stores.length ? stores.join(" · ") : qsTr("—")
    }
    readonly property bool isOwned: root.selectedVariant
        && ["MANUAL", "PLATFORM_SYNC"].indexOf(root.selectedVariant.libraryStatus) >= 0
    readonly property string ownershipText: {
        const game = root.game
        const variant = root.selectedVariant
        const store = variant && variant.store
            ? String(variant.store)
            : ((game && game.availableStores && game.availableStores[0]) || "")
        if (store && root.isOwned)
            return qsTr("OWNED ON %1").arg(store.toUpperCase())
        if (store)
            return store.toUpperCase()
        return root.isOwned ? qsTr("IN LIBRARY") : qsTr("NOT OWNED")
    }
    readonly property string membershipText: {
        const sub = ShellStore.subscription
        if (sub && sub.membershipTier)
            return String(sub.membershipTier).toUpperCase()
        const user = ShellStore.authSession && ShellStore.authSession.user
        if (user && user.membershipTier)
            return String(user.membershipTier).toUpperCase()
        return ""
    }
    readonly property string resolutionText: {
        const raw = String(root.streamSettings.resolution || "")
        if (raw.indexOf("x") > 0) {
            const height = Number(raw.split("x")[1])
            if (height >= 2160)
                return "4K"
            if (height > 0)
                return height + "p"
        }
        return raw || qsTr("Auto")
    }
    readonly property string fpsText: {
        const fps = Number(root.streamSettings.fps || 0)
        return fps > 0 ? qsTr("%1 fps").arg(fps) : qsTr("Auto")
    }
    readonly property string codecText: {
        const codec = String(root.streamSettings.codec || "")
        if (!codec || codec.toLowerCase() === "auto")
            return qsTr("Auto")
        return codec.toUpperCase()
    }
    readonly property string regionText: {
        const region = String(root.streamSettings.region || "")
        return region ? region.toUpperCase() : qsTr("AUTOMATIC REGION")
    }
    readonly property string streamSummary: [resolutionText, fpsText, codecText].join(" \u00B7 ")
    readonly property bool launchReady: ShellStore.signedIn && root.gameAvailable
    readonly property bool launchBusy: ShellStore.cloudMutationBusy || ShellStore.launchInspectRequestId !== ""

    function revealFocusedControl() {
        if (!root.opened || !root.Window.window)
            return
        const item = root.Window.window.activeFocusItem
        let ancestor = item
        while (ancestor && ancestor !== detailsColumn)
            ancestor = ancestor.parent
        if (!ancestor)
            return
        const top = item.mapToItem(detailsFlick.contentItem, 0, 0).y
        const margin = DesktopTokens.px(8)
        const maximumY = Math.max(0, detailsFlick.contentHeight - detailsFlick.height)
        if (top < detailsFlick.contentY + margin)
            detailsFlick.contentY = Math.max(0, top - margin)
        else if (top + item.height > detailsFlick.contentY + detailsFlick.height - margin)
            detailsFlick.contentY = Math.min(maximumY, top + item.height + margin - detailsFlick.height)
    }
    Connections {
        target: root.Window.window
        function onActiveFocusItemChanged() { Qt.callLater(root.revealFocusedControl) }
    }

    function tune() { AppController.navigate("settings-streaming") }
    readonly property var summaryCards: [
        {label:qsTr("Stream"), title:resolutionText + " \u00B7 " + fpsText, detail:codecText + " \u00B7 " + String(streamSettings.colorQuality || "8bit_420").replace("_", " "), mono:true},
        {label:qsTr("Region"), title:regionLabel(), detail:qsTr("Region selected at launch"), mono:false},
        {label:qsTr("Membership"), title:membershipText || qsTr("Membership"), detail:ShellStore.subscription && ShellStore.subscription.remainingHours !== undefined
            ? qsTr("%1 h remaining").arg(Math.max(0, Number(ShellStore.subscription.remainingHours)).toFixed(1)) : qsTr("Entitlements checked at launch"), mono:false}
    ]
    function regionLabel() {
        const value = String(streamSettings.region || "")
        const regions = ShellStore.regions || []
        for (const region of regions)
            if (region.url === value || region.name === value) return String(region.name)
        return value ? qsTr("Selected region") : qsTr("Automatic region")
    }
    Rectangle {
        anchors.fill: parent; color: "#A6040D10"; opacity: reveal.progress
        MouseArea {
            anchors.fill: parent; acceptedButtons: Qt.AllButtons
            hoverEnabled: true; preventStealing: true
            onClicked: root.closeRequested()
            onWheel: wheel => wheel.accepted = true
        }
    }
    Rectangle {
        id: dialog
        objectName: "gameDetailsDialog"
        opacity: reveal.progress
        scale: reveal.zoom
        transformOrigin: Item.Center
        anchors.centerIn: parent
        width: root.dialogWidth; height: root.dialogHeight
        radius: DesktopTokens.px(24); color: Theme.shell; border.width: 1; border.color: Theme.seam
        // Swallow blank-space clicks inside the modal, never activate its scrim.
        MouseArea { anchors.fill: parent; acceptedButtons: Qt.AllButtons; onWheel: wheel => wheel.accepted = true }
        Flickable {
            id: detailsFlick
            objectName: "gameDetailsScroll"
            anchors.fill: parent
            contentWidth: width; contentHeight: detailsColumn.implicitHeight
            clip: true; boundsBehavior: Flickable.StopAtBounds
            flickableDirection: Flickable.VerticalFlick
            onHeightChanged: Qt.callLater(root.revealFocusedControl)
            onContentHeightChanged: Qt.callLater(root.revealFocusedControl)
            ScrollBar.vertical: ScrollBar {
                policy: detailsFlick.contentHeight > detailsFlick.height ? ScrollBar.AlwaysOn : ScrollBar.AlwaysOff
            }
            Column {
                id: detailsColumn
                width: parent.width
                Item {
                    width: parent.width
                    height: Math.max(headerInfo.implicitHeight + DesktopTokens.px(48),
                        Math.min(DesktopTokens.px(280), root.maximumDialogHeight * 0.38))
                    RoundedArtwork {
                        anchors.fill: parent; artwork: DesktopTokens.artworkUrl(root.game, true)
                        cornerRadius: DesktopTokens.px(24); scrimStart: 0.1; fallbackColor: Theme.shell
                    }
                    Rectangle {
                        anchors.fill: parent
                        gradient: Gradient {
                            GradientStop { position: 0.25; color: "transparent" }
                            GradientStop { position: 1; color: Theme.shell }
                        }
                    }
                    Column {
                        id: headerInfo
                        x: DesktopTokens.px(28); anchors.bottom: parent.bottom; anchors.bottomMargin: DesktopTokens.px(24)
                        width: parent.width - DesktopTokens.px(56); spacing: DesktopTokens.px(10)
                        Row {
                            spacing: DesktopTokens.px(8)
                            Rectangle {
                                anchors.verticalCenter: parent.verticalCenter
                                width: DesktopTokens.px(7); height: width; radius: width / 2
                                color: root.launchReady ? Theme.focus : DesktopTokens.amber
                            }
                            Text {
                                anchors.verticalCenter: parent.verticalCenter
                                text: root.ownershipText
                                color: Theme.label; opacity: 0.86
                                font.family: Theme.monoFont; font.pixelSize: DesktopTokens.smallSize
                                font.weight: Font.DemiBold; font.letterSpacing: 1.2
                            }
                        }
                        Text {
                            width: parent.width; text: root.game ? String(root.game.title || qsTr("Game")) : qsTr("Game")
                            color: Theme.label; font.family: Theme.displayFont
                            font.pixelSize: DesktopTokens.px(40); font.weight: Font.Black; font.letterSpacing: -0.8
                            lineHeight: 0.95; lineHeightMode: Text.ProportionalHeight
                            wrapMode: Text.WordWrap; maximumLineCount: 2; elide: Text.ElideRight
                        }
                        Text {
                            width: parent.width
                            wrapMode: Text.WordWrap; maximumLineCount: 2; elide: Text.ElideRight
                            text: [root.game && (root.game.publisherName || root.game.publisher) || "", root.lastPlayedText, root.game && root.game.hoursPlayed ? qsTr("%1 h").arg(root.game.hoursPlayed) : ""].filter(Boolean).join(" \u00B7 ")
                            color: Theme.textMuted; font.family: Theme.bodyFont; font.pixelSize: DesktopTokens.captionSize
                        }
                    }
                }
                Item {
                    width: parent.width
                    height: bodyContent.implicitHeight + DesktopTokens.px(24)
                    Column {
                        id: bodyContent
                        objectName: "gameDetailsBody"
                        x: DesktopTokens.px(28)
                        width: parent.width - DesktopTokens.px(56)
                        spacing: DesktopTokens.px(20)
                        Column {
                            objectName: "gameDetailsReadiness"
                            width: parent.width
                            spacing: DesktopTokens.px(8)
                            Text {
                                id: readinessNoticeLabel
                                objectName: "catalogReadinessNotice"
                                width: parent.width
                                text: I18n.source(ShellStore.cloudMutationMessage || ShellStore.selectedLaunchDecision.message || ShellStore.readinessNotice(root.game), I18n.revision)
                                visible: text !== ""
                                wrapMode: Text.WordWrap
                                leftPadding: DesktopTokens.px(14)
                                Rectangle {
                                    width: DesktopTokens.px(3); height: parent.height; radius: width / 2
                                    color: ShellStore.cloudMutationState === "unconfirmed" ? DesktopTokens.danger
                                        : root.gameAvailable ? Theme.seam : Theme.focus
                                }
                                color: ShellStore.cloudMutationState === "unconfirmed" ? (Theme.lightMode ? Qt.darker(DesktopTokens.danger, 2) : DesktopTokens.danger) : Theme.textMuted
                                font.family: Theme.bodyFont
                                font.pixelSize: DesktopTokens.captionSize
                            }
                            visible: storeVariants.count > 1 || readinessNoticeLabel.text !== ""
                            Text {
                                text: qsTr("Choose platform")
                                visible: storeVariants.count > 1
                                color: Theme.textMuted
                                font.family: Theme.bodyFont
                                font.pixelSize: DesktopTokens.captionSize
                                font.weight: Font.DemiBold
                            }
                            Flow {
                                visible: storeVariants.count > 1
                                width: parent.width
                                spacing: DesktopTokens.px(8)
                                Repeater {
                                    id: storeVariants
                                    model: root.game ? root.game.variants || [] : []
                                    DesktopButton {
                                        id: platformButton
                                        required property var modelData
                                        required property int index
                                        objectName: "desktopStoreVariant" + index
                                        text: String(modelData.store || qsTr("Unknown"))
                                        height: DesktopTokens.px(36)
                                        cornerRadius: DesktopTokens.px(12)
                                        tinted: true
                                        leftPadding: DesktopTokens.px(12)
                                        rightPadding: DesktopTokens.px(14)
                                        font.pixelSize: DesktopTokens.captionSize
                                        implicitWidth: platformContents.implicitWidth + leftPadding + rightPadding
                                        checkable: true
                                        autoExclusive: true
                                        checked: root.game ? index === Number(root.game.selectedVariantIndex || 0) : false
                                        Accessible.description: ["MANUAL", "PLATFORM_SYNC"].indexOf(modelData.libraryStatus) >= 0
                                            ? qsTr("Owned") : modelData.libraryStatus === "NOT_OWNED" ? qsTr("Not owned") : qsTr("Ownership unconfirmed")
                                        onClicked: root.variantSelected(index)
                                        Keys.onReturnPressed: root.variantSelected(index)
                                        Keys.onEnterPressed: root.variantSelected(index)
                                        contentItem: Row {
                                            id: platformContents
                                            spacing: DesktopTokens.px(8)
                                            Rectangle {
                                                anchors.verticalCenter: parent.verticalCenter
                                                width: DesktopTokens.px(26)
                                                height: width
                                                radius: DesktopTokens.px(5)
                                                visible: platformLogo.source.toString() !== ""
                                                color: platformButton.checked || Theme.lightMode ? "#202634" : "transparent"
                                                Image {
                                                    id: platformLogo
                                                    objectName: "desktopStoreLogo" + platformButton.index
                                                    anchors.centerIn: parent
                                                    width: DesktopTokens.px(20)
                                                    height: width
                                                    source: DesktopTokens.storeIconUrl(platformButton.modelData.store)
                                                    sourceSize: Qt.size(width * Screen.devicePixelRatio, height * Screen.devicePixelRatio)
                                                    fillMode: Image.PreserveAspectFit
                                                }
                                            }
                                            Text {
                                                anchors.verticalCenter: parent.verticalCenter
                                                text: platformButton.text
                                                font: platformButton.font
                                                color: Theme.label
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        RowLayout {
                            id: actionRow
                            objectName: "gameDetailsPrimaryActions"
                            width: parent.width
                            spacing: DesktopTokens.px(4)
                            Button {
                                id: primaryAction
                                objectName: "desktopGamePlay"
                                readonly property bool ready: root.launchReady
                                readonly property bool busy: root.launchBusy
                                readonly property color ink: ready ? Theme.focusText : Theme.label
                                property real pulse: 1
                                Layout.fillWidth: true; Layout.minimumWidth: 0; Layout.preferredHeight: DesktopTokens.px(64)
                                Layout.rightMargin: DesktopTokens.px(8)
                                leftPadding: DesktopTokens.px(12); rightPadding: DesktopTokens.px(18); topPadding: 0; bottomPadding: 0
                                focusPolicy: Qt.StrongFocus
                                text: ShellStore.selectedGameActionLabel()
                                enabled: root.game !== null && !busy
                                Accessible.name: text
                                Accessible.description: ready ? root.streamSummary : ""
                                onClicked: root.playRequested()
                                SequentialAnimation on pulse {
                                    running: primaryAction.busy && !AppController.reducedMotion
                                    loops: Animation.Infinite
                                    NumberAnimation { to: 0.45; duration: 620; easing.type: Easing.InOutSine }
                                    NumberAnimation { to: 1; duration: 620; easing.type: Easing.InOutSine }
                                }
                                background: Rectangle {
                                    radius: DesktopTokens.px(18)
                                    color: primaryAction.ready ? Theme.focus : DesktopTokens.raisedStrong
                                    border.width: primaryAction.ready ? 0 : 1
                                    border.color: Theme.seam
                                    opacity: primaryAction.enabled || primaryAction.busy ? 1 : 0.5
                                    scale: primaryAction.down && !AppController.reducedMotion ? 0.98 : 1
                                    Behavior on color { ColorAnimation { duration: DesktopTokens.quickDuration } }
                                    Behavior on scale { NumberAnimation { duration: DesktopTokens.quickDuration; easing.type: Easing.OutCubic } }
                                    Rectangle {
                                        anchors.fill: parent; radius: parent.radius
                                        color: primaryAction.ready ? "#FFFFFF" : Theme.label
                                        opacity: primaryAction.hovered && primaryAction.enabled ? (primaryAction.ready ? 0.16 : 0.07) : 0
                                        Behavior on opacity { NumberAnimation { duration: DesktopTokens.quickDuration } }
                                    }
                                    Rectangle {
                                        anchors.fill: parent; anchors.margins: 1; radius: parent.radius - 1
                                        color: "transparent"; border.width: 1
                                        border.color: "#38FFFFFF"
                                        visible: primaryAction.ready
                                    }
                                    Rectangle {
                                        anchors.fill: parent; anchors.margins: -3; radius: parent.radius + 3
                                        color: "transparent"; border.width: 2; border.color: DesktopTokens.focus
                                        visible: primaryAction.activeFocus
                                    }
                                }
                                contentItem: RowLayout {
                                    spacing: DesktopTokens.px(14)
                                    opacity: primaryAction.enabled || primaryAction.busy ? 1 : 0.5
                                    Item {
                                        visible: primaryAction.ready
                                        Layout.preferredWidth: DesktopTokens.px(40); Layout.preferredHeight: DesktopTokens.px(40)
                                        Layout.alignment: Qt.AlignVCenter
                                        Rectangle {
                                            anchors.fill: parent; radius: width / 2
                                            color: Qt.rgba(primaryAction.ink.r, primaryAction.ink.g, primaryAction.ink.b, 0.14)
                                        }
                                        // Play triangle nudged right of centre: a centred triangle reads as left-heavy.
                                        Shape {
                                            anchors.centerIn: parent
                                            anchors.horizontalCenterOffset: DesktopTokens.px(2)
                                            width: DesktopTokens.px(14); height: DesktopTokens.px(16)
                                            layer.enabled: true; layer.samples: 4
                                            ShapePath {
                                                fillColor: primaryAction.ink; strokeColor: primaryAction.ink
                                                strokeWidth: 2; joinStyle: ShapePath.RoundJoin
                                                startX: 1; startY: 1
                                                PathLine { x: DesktopTokens.px(14) - 1; y: DesktopTokens.px(16) / 2 }
                                                PathLine { x: 1; y: DesktopTokens.px(16) - 1 }
                                                PathLine { x: 1; y: 1 }
                                            }
                                        }
                                    }
                                    ColumnLayout {
                                        Layout.fillWidth: true; Layout.minimumWidth: 0; Layout.alignment: Qt.AlignVCenter
                                        Layout.leftMargin: primaryAction.ready ? 0 : DesktopTokens.px(6)
                                        spacing: DesktopTokens.px(2)
                                        Text {
                                            Layout.fillWidth: true
                                            text: primaryAction.text; elide: Text.ElideRight
                                            color: primaryAction.ink
                                            opacity: primaryAction.busy ? primaryAction.pulse : 1
                                            font.family: Theme.displayFont; font.pixelSize: DesktopTokens.px(19)
                                            font.weight: Font.Black; font.letterSpacing: -0.2
                                        }
                                        Text {
                                            Layout.fillWidth: true
                                            visible: primaryAction.ready
                                            text: root.streamSummary; elide: Text.ElideRight
                                            color: primaryAction.ink; opacity: 0.72
                                            font.family: Theme.monoFont; font.pixelSize: DesktopTokens.monoSize
                                        }
                                    }
                                    KeyboardGlyph {
                                        visible: primaryAction.enabled
                                        Layout.alignment: Qt.AlignVCenter
                                        shortcut: "Enter"
                                        keySize: DesktopTokens.px(22)
                                        ink: primaryAction.ink
                                        opacity: 0.8
                                        Accessible.name: qsTr("ENTER")
                                    }
                                }
                            }
                            DesktopButton {
                                Layout.preferredWidth: DesktopTokens.px(48); Layout.preferredHeight: DesktopTokens.px(48); Layout.alignment: Qt.AlignVCenter
                                quiet: true; cornerRadius: DesktopTokens.px(14); glyphSize: DesktopTokens.px(18)
                                themedGlyph: "star"; leftPadding: 0; rightPadding: 0
                                Accessible.name: root.game && ShellStore.isCloudFavorite(root.game) ? qsTr("Remove from GeForce NOW favorites") : qsTr("Add to GeForce NOW favorites")
                                ToolTip.visible: hovered; ToolTip.text: Accessible.name
                                enabled: ShellStore.signedIn && !ShellStore.cloudMutationBusy
                                onClicked: if (root.game) ShellStore.toggleCloudFavorite(root.game)
                            }
                            DesktopButton {
                                Layout.preferredWidth: DesktopTokens.px(48); Layout.preferredHeight: DesktopTokens.px(48); Layout.alignment: Qt.AlignVCenter
                                quiet: true; cornerRadius: DesktopTokens.px(14); glyphSize: DesktopTokens.px(18)
                                themedGlyph: "folder"; leftPadding: 0; rightPadding: 0
                                Accessible.name: qsTr("Collections")
                                ToolTip.visible: hovered; ToolTip.text: Accessible.name
                                onClicked: collectionMenu.popup()
                                Menu {
                                    id: collectionMenu
                                    MenuItem { text: qsTr("Pin to Home"); checkable: true; checked: root.game && ShellStore.isFavorite(root.game); onTriggered: if (root.game) ShellStore.toggleFavorite(root.game) }
                                }
                            }
                            DesktopButton {
                                Layout.preferredWidth: DesktopTokens.px(48); Layout.preferredHeight: DesktopTokens.px(48); Layout.alignment: Qt.AlignVCenter
                                quiet: true; cornerRadius: DesktopTokens.px(14); glyphSize: DesktopTokens.px(18)
                                themedGlyph: "more"; leftPadding: 0; rightPadding: 0
                                Accessible.name: qsTr("More game actions")
                                ToolTip.visible: hovered; ToolTip.text: Accessible.name
                                onClicked: moreMenu.popup()
                                Menu {
                                    id: moreMenu
                                    MenuItem { text: qsTr("Stream settings"); onTriggered: root.tune() }
                                    MenuItem { text: qsTr("Close details"); onTriggered: root.closeRequested() }
                                }
                            }
                        }
                        CloudLibraryActions {
                            width: parent.width
                            game: root.game
                            showFavorites: false
                            showStatus: false
                            quiet: true
                        }
                        Rectangle {
                            id: summaryGrid
                            objectName: "gameDetailsSummary"
                            width: parent.width
                            height: summaryLayout.implicitHeight + DesktopTokens.px(32)
                            radius: DesktopTokens.px(16); color: DesktopTokens.raised
                            GridLayout {
                                id: summaryLayout
                                anchors.fill: parent; anchors.margins: DesktopTokens.px(16)
                                columns: summaryGrid.width < DesktopTokens.px(620) ? 2 : 4
                                uniformCellWidths: columns === 2
                                columnSpacing: DesktopTokens.px(20); rowSpacing: DesktopTokens.px(16)
                                Repeater {
                                    model: root.summaryCards
                                    delegate: Item {
                                        id: specCell
                                        required property var modelData
                                        objectName: "gameDetailsSummaryCard"
                                        Layout.fillWidth: true; Layout.minimumWidth: 0; Layout.preferredWidth: DesktopTokens.px(160)
                                        Layout.alignment: Qt.AlignTop
                                        implicitHeight: specColumn.implicitHeight
                                        Layout.preferredHeight: specColumn.implicitHeight
                                        Column {
                                            id: specColumn
                                            width: parent.width; spacing: DesktopTokens.px(3)
                                            Text { width: parent.width; text: specCell.modelData.label; elide: Text.ElideRight; color: Theme.textMuted; font.family: Theme.bodyFont; font.pixelSize: DesktopTokens.smallSize; font.weight: Font.DemiBold }
                                            Text { width: parent.width; text: specCell.modelData.title; elide: Text.ElideRight; color: Theme.label; font.family: specCell.modelData.mono ? Theme.monoFont : Theme.bodyFont; font.pixelSize: DesktopTokens.bodySize; font.weight: Font.Bold }
                                            Text { width: parent.width; text: specCell.modelData.detail; elide: Text.ElideRight; color: Theme.textMuted; font.family: specCell.modelData.mono ? Theme.monoFont : Theme.bodyFont; font.pixelSize: DesktopTokens.smallSize }
                                        }
                                    }
                                }
                                DesktopButton {
                                    objectName: "gameDetailsTune"
                                    Layout.alignment: Qt.AlignVCenter | Qt.AlignRight; Layout.preferredHeight: DesktopTokens.px(40)
                                    quiet: true; cornerRadius: DesktopTokens.px(12)
                                    font.pixelSize: DesktopTokens.captionSize
                                    text: qsTr("Tune"); themedGlyph: "sliders"; leftPadding: DesktopTokens.px(12); rightPadding: DesktopTokens.px(12)
                                    onClicked: root.tune()
                                }
                            }
                        }
                    }
                }
            }
        }
        DesktopButton {
            objectName: "gameDetailsClose"
            anchors.right: parent.right; anchors.rightMargin: -width / 2
            anchors.top: parent.top; anchors.topMargin: -height / 2
            width: DesktopTokens.px(36); height: DesktopTokens.px(36); themedGlyph: "close"; leftPadding: 0; rightPadding: 0
            cornerRadius: width / 2
            Accessible.name: qsTr("Close details")
            onClicked: root.closeRequested()
        }
    }
    Keys.onEscapePressed: root.closeRequested()
    Keys.onReturnPressed: if (primaryAction.enabled) root.playRequested()
}
