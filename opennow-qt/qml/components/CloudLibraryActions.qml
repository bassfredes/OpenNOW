import QtQuick
import QtQuick.Controls
import OpenNOW

Column {
    id: root
    objectName: "cloudLibraryActions"
    property var game: ShellStore.selectedGame
    property bool showFavorites: true
    property bool showStatus: true
    property bool quiet: false
    readonly property var variant: game && (game.variants || [])[Number(game.selectedVariantIndex || 0)]
    readonly property bool owned: Boolean(variant) && ["MANUAL", "PLATFORM_SYNC"].indexOf(variant.libraryStatus) >= 0
    spacing: DesktopTokens.px(8)
    Text {
        width: parent.width
        objectName: "cloudLibraryStatus"
        text: I18n.source(ShellStore.cloudMutationMessage || ShellStore.selectedLaunchDecision.message || "", I18n.revision)
        visible: root.showStatus && text !== ""
        wrapMode: Text.WordWrap
        color: ShellStore.cloudMutationState === "unconfirmed" ? (Theme.lightMode ? Qt.darker(DesktopTokens.danger, 2) : DesktopTokens.danger) : Theme.textMuted
        font.family: Theme.bodyFont
        font.pixelSize: DesktopTokens.captionSize
        Accessible.role: Accessible.StaticText
    }
    Flow {
        width: parent.width
        spacing: DesktopTokens.px(8)
        DesktopButton {
            quiet: root.quiet
            objectName: "cloudOwnershipAdd"
            visible: Boolean(root.variant) && root.variant.libraryStatus === "NOT_OWNED" && ShellStore.selectedLaunchDecision.status !== "ownership_required"
            enabled: ShellStore.signedIn && !ShellStore.cloudMutationBusy
            text: qsTr("I own this game")
            onClicked: ShellStore.requestOwnershipConfirmation("add")
        }
        DesktopButton {
            quiet: root.quiet
            objectName: "cloudFavoriteAction"
            visible: root.showFavorites
            enabled: ShellStore.signedIn && !ShellStore.cloudMutationBusy
            text: ShellStore.isCloudFavorite(root.game) ? qsTr("Remove from GeForce NOW favorites") : qsTr("Add to GeForce NOW favorites")
            onClicked: ShellStore.toggleCloudFavorite(root.game)
        }
        DesktopButton {
            quiet: root.quiet
            objectName: "cloudOwnershipRemove"
            visible: root.owned
            enabled: !ShellStore.cloudMutationBusy
            text: qsTr("Remove %1 ownership").arg(root.variant ? DesktopTokens.storeLabel(root.variant.store) : "")
            onClicked: ShellStore.requestOwnershipConfirmation("remove")
        }
        DesktopButton {
            quiet: root.quiet
            objectName: "cloudOwnershipSelect"
            visible: root.owned && root.variant.librarySelected !== true
            enabled: !ShellStore.cloudMutationBusy
            text: qsTr("Use this store version")
            onClicked: ShellStore.selectPreferredVariant()
        }
        DesktopButton {
            quiet: root.quiet
            objectName: "cloudLibraryRefresh"
            text: qsTr("Refresh status")
            enabled: ShellStore.signedIn && !ShellStore.cloudMutationBusy
            onClicked: ShellStore.refreshSelectedMetadata()
        }
        DesktopButton {
            quiet: root.quiet
            text: qsTr("Game accounts")
            onClicked: AppController.navigate("game-accounts")
        }
    }
    Dialog {
        id: confirmation
        objectName: "cloudOwnershipConfirmation"
        parent: Overlay.overlay
        anchors.centerIn: parent
        width: Math.min(DesktopTokens.px(520), parent ? parent.width - DesktopTokens.px(32) : DesktopTokens.px(520))
        modal: true
        focus: true
        visible: ShellStore.ownershipConfirmation !== null
        title: ShellStore.ownershipConfirmation && ShellStore.ownershipConfirmation.action === "remove"
            ? qsTr("Remove this store version?") : qsTr("Do you already own this game?")
        closePolicy: Popup.CloseOnEscape
        onClosed: ShellStore.ownershipConfirmation = null
        background: Rectangle { color: Theme.shell; radius: DesktopTokens.px(16); border.color: Theme.seam }
        header: Label {
            text: confirmation.title
            color: Theme.label
            font.family: Theme.bodyFont
            font.pixelSize: DesktopTokens.headingSize
            font.weight: Font.Bold
            padding: DesktopTokens.px(16)
            wrapMode: Text.WordWrap
        }
        contentItem: Column {
            spacing: DesktopTokens.px(16)
            Text {
                width: parent.width
                wrapMode: Text.WordWrap
                color: Theme.label
                font.family: Theme.bodyFont
                font.pixelSize: DesktopTokens.bodySize
                text: {
                    const target = ShellStore.ownershipConfirmation
                    if (!target) return ""
                    const store = DesktopTokens.storeLabel(target.store)
                    if (target.action === "remove") return qsTr("Remove the %1 version from your GeForce NOW library? This does not uninstall the game or revoke your license. A later store sync may restore it.").arg(store)
                    return qsTr("This adds the %1 version to your GeForce NOW library. It does not buy the game or grant a license. You must already own it on %1.").arg(store)
                }
            }
            Flow {
                width: parent.width
                spacing: DesktopTokens.px(8)
                DesktopButton { objectName: "cloudOwnershipCancel"; text: qsTr("Cancel"); onClicked: ShellStore.ownershipConfirmation = null }
                DesktopButton {
                    objectName: "cloudOwnershipConfirm"
                    primary: true
                    text: ShellStore.ownershipConfirmation && ShellStore.ownershipConfirmation.action === "remove" ? qsTr("Remove ownership") : qsTr("Confirm existing ownership")
                    onClicked: ShellStore.confirmOwnership()
                }
            }
        }
    }
}
