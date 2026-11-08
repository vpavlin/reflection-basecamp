// p2panda for Logos Basecamp: the view of p2panda_core, the device's shared p2panda node.
//
// Shows the networks and the apps using them, and the topics bridged to Logos Delivery. Apps can
// only ask for a bridge; it starts when the user approves it here, because it spends this
// device's Logos messages (RLN).
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Logos.Theme
import Logos.Controls

Item {
    id: root
    width: 1000
    height: 680

    // ── colours: design-system tokens with fallbacks (a missing token is undefined, not fatal)
    function tok(v, fb) { return (v === undefined || v === null) ? fb : v }
    readonly property var pal: (typeof Theme !== "undefined" && Theme.palette) ? Theme.palette : ({})
    readonly property color cBg: tok(pal.background, "#141416")
    readonly property color cSurface: tok(pal.surface, "#1c1c20")
    readonly property color cRaised: tok(pal.surfaceRaised, "#26262c")
    readonly property color cText: tok(pal.text, "#ececf1")
    readonly property color cSub: tok(pal.textSecondary, "#a8a8b3")
    readonly property color cFaint: tok(pal.textTertiary, "#74747f")
    readonly property color cLine: tok(pal.borderHairline, "#2e2e36")
    readonly property color cAccent: tok(pal.primary, "#ff7a3d")
    readonly property color cOk: tok(pal.success, "#5fbf7f")
    readonly property color cBad: tok(pal.error, "#ef6f6c")
    readonly property color cCode: tok(pal.success, "#7fc79a")
    readonly property color cLink: "#8ab4f8"
    readonly property color cEmph: "#e9b97a"
    readonly property string viewVersion: "0.1.0"


    // ── core bridge (never blocking: callModuleAsync, or callModule deferred a frame)
    readonly property string coreName: "p2panda_core"
    readonly property int callTimeoutMs: 20000
    function _deliver(cb, raw) {
        if (!cb) return
        try { cb(raw === undefined || raw === null ? "" : raw) } catch (e) { console.warn("reflection view: " + e) }
    }
    function callVia(module, method, args, cb) {
        var a = args || []
        if (typeof logos === "undefined" || logos === null) { Qt.callLater(function () { root._deliver(cb, "") }); return }
        if (typeof logos.callModuleAsync === "function") {
            try { logos.callModuleAsync(module, method, a, function (res) { root._deliver(cb, res) }, root.callTimeoutMs) }
            catch (e) { Qt.callLater(function () { root._deliver(cb, "") }) }
            return
        }
        Qt.callLater(function () {
            var raw = ""
            try { raw = (typeof logos.callModule === "function") ? logos.callModule(module, method, a) : "" } catch (e) { raw = "" }
            root._deliver(cb, raw)
        })
    }
    function core(method, args, cb) { root.callVia(root.coreName, method, args, cb) }
    // Results may arrive as JSON, or JSON-quoted once or twice.
    function parse(raw) {
        var v = raw
        for (var i = 0; i < 3 && typeof v === "string"; i++) {
            var s = v.trim()
            if (s === "") return null
            try { v = JSON.parse(s) } catch (e) { return null }
        }
        return (v && typeof v === "object") ? v : null
    }

    property var status: null
    property var bridges: []
    property bool coreSeen: false
    property string notice: ""
    property bool noticeBad: false
    function say(text, bad) { root.notice = text; root.noticeBad = !!bad; noticeTimer.restart() }
    Timer { id: noticeTimer; interval: 5000; onTriggered: root.notice = "" }

    function refresh() {
        root.core("status", [], function (raw) {
            var s = root.parse(raw)
            if (s && s.ok) { root.status = s; root.coreSeen = true }
        })
        root.core("bridges", [], function (raw) {
            var b = root.parse(raw)
            if (b && b.ok) root.bridges = b.bridges || []
        })
    }
    Timer { interval: 2000; running: true; repeat: true; triggeredOnStart: true; onTriggered: root.refresh() }

    function confirm(b, on) {
        root.core("bridge_confirm", [b.network, b.topic, on ? "true" : "false"], function (raw) {
            var r = root.parse(raw)
            if (!r || !r.ok) { root.say((r && r.error) || "Could not change the bridge", true); return }
            root.say(on ? "Bridged to Logos: " + (b.label || b.topic.substring(0, 8)) : "Not bridged any more: " + (b.label || b.topic.substring(0, 8)))
            root.refresh()
        })
    }
    function shortId(hex) { return hex ? hex.substring(0, 8) + "..." + hex.substring(hex.length - 4) : "" }
    function appCount(network) {
        var n = 0
        var subs = (root.status && root.status.subscriptions) || []
        for (var i = 0; i < subs.length; i++) if (subs[i].network === network) n++
        return n
    }

    component RLabel: Text { color: root.cSub; font.pixelSize: 12; textFormat: Text.PlainText; wrapMode: Text.Wrap }

    Rectangle { anchors.fill: parent; color: root.cBg }

    ScrollView {
        anchors.fill: parent
        anchors.margins: 20
        clip: true
        contentWidth: availableWidth

        ColumnLayout {
            width: parent ? parent.width : 600
            spacing: 12

            Text { text: "p2panda"; color: root.cText; font.pixelSize: 22; font.bold: true; textFormat: Text.PlainText }
            RLabel {
                Layout.fillWidth: true
                text: "The shared p2panda node of this device. Apps that sync over p2panda (like Reflection in p2panda mode) use it; it reaches the same network as the upstream apps. Topics can also be bridged to Logos Delivery, so Logos users see them too: an app can only ask for that, you decide here."
            }
            RLabel { visible: !root.coreSeen; text: "Waiting for p2panda_core..."; color: root.cFaint }

            // bridges
            Text { text: "Bridges to Logos"; color: root.cText; font.pixelSize: 16; font.bold: true; textFormat: Text.PlainText; Layout.topMargin: 6 }
            RLabel { visible: root.bridges.length === 0 && root.coreSeen; text: "Nothing bridged, nothing asked for."; color: root.cFaint }
            Repeater {
                model: root.bridges
                delegate: Rectangle {
                    required property var modelData
                    Layout.fillWidth: true
                    Layout.preferredHeight: bridgeRow.implicitHeight + 20
                    radius: 8
                    color: root.cSurface
                    border.width: 1
                    border.color: modelData.state === "pending" ? root.cAccent : root.cLine
                    RowLayout {
                        id: bridgeRow
                        anchors.fill: parent
                        anchors.margins: 10
                        spacing: 10
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 2
                            Text {
                                Layout.fillWidth: true
                                text: (modelData.label || "Untitled") + "  -  " + modelData.app
                                color: root.cText; font.pixelSize: 14; elide: Text.ElideRight; textFormat: Text.PlainText
                            }
                            RLabel {
                                Layout.fillWidth: true
                                font.pixelSize: 11
                                text: modelData.state === "pending"
                                      ? "Asks to be carried over Logos Delivery as well. Its edits will be relayed to Logos users, paid from this device's Logos messages."
                                      : "Bridged: edits go both ways between p2panda and Logos (topic " + root.shortId(modelData.topic) + ")."
                            }
                        }
                        LogosButton {
                            visible: modelData.state === "pending"
                            text: "Allow"
                            Layout.preferredWidth: 80; Layout.preferredHeight: 32
                            onClicked: root.confirm(modelData, true)
                        }
                        LogosButton {
                            text: modelData.state === "pending" ? "Refuse" : "Stop"
                            Layout.preferredWidth: 80; Layout.preferredHeight: 32
                            onClicked: root.confirm(modelData, false)
                        }
                    }
                }
            }

            // networks
            Text { text: "Networks"; color: root.cText; font.pixelSize: 16; font.bold: true; textFormat: Text.PlainText; Layout.topMargin: 10 }
            RLabel { visible: root.coreSeen && (!root.status || (root.status.networks || []).length === 0); text: "No app has opened a p2panda network yet."; color: root.cFaint }
            Repeater {
                model: (root.status && root.status.networks) ? root.status.networks : []
                delegate: Rectangle {
                    required property var modelData
                    Layout.fillWidth: true
                    Layout.preferredHeight: netCol.implicitHeight + 20
                    radius: 8
                    color: root.cSurface
                    border.width: 1
                    border.color: root.cLine
                    ColumnLayout {
                        id: netCol
                        anchors.fill: parent
                        anchors.margins: 10
                        spacing: 2
                        Text { text: modelData.app + " network"; color: root.cText; font.pixelSize: 14; textFormat: Text.PlainText }
                        RLabel {
                            Layout.fillWidth: true
                            font.pixelSize: 11
                            text: "Network " + root.shortId(modelData.network) + ", identity " + root.shortId(modelData.author)
                                  + ". Open topics: " + root.appCount(modelData.network) + ". Bridged: " + modelData.bridged + "."
                                  + (modelData.logos ? (" Logos messages sent this epoch: " + (function (c) { return (c.ops||0)+(c.snapshots||0)+(c.heads||0)+(c.repairs||0)+(c.relayed||0)+(c.presence||0)+(c.cursors||0) })(modelData.logos.sent_this_epoch) + ".") : "")
                        }
                    }
                }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: noticeText.implicitHeight + 12
                visible: root.notice !== ""
                radius: 6
                color: root.noticeBad ? Qt.darker(root.cBad, 2.2) : root.cRaised
                Text { id: noticeText; anchors.fill: parent; anchors.margins: 6; text: root.notice; color: root.noticeBad ? root.cBad : root.cText; wrapMode: Text.Wrap; textFormat: Text.PlainText }
            }
            RLabel { text: "view " + root.viewVersion; color: root.cFaint; font.pixelSize: 10 }
        }
    }
}
