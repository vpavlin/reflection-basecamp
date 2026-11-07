// Reflection for Logos Basecamp: collaborative notes, synced over Logos Delivery.
//
// A thin view: all state lives in reflection_core. The view polls `state` once a second and
// sends the editor's whole text with `set_text`, together with the version the text was
// edited from, so the core can merge it with changes from other people (see engine.rs).
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
    readonly property string viewVersion: "0.1.0"

    // ── core bridge (never blocking: callModuleAsync, or callModule deferred a frame)
    readonly property string coreName: "reflection_core"
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

    // ── state
    property var st: ({ docs: [], transport: {}, node: "starting", current: null })
    property bool coreSeen: false
    property int missedPolls: 0
    property bool pollBusy: false
    property bool pollAgain: false
    property string notice: ""
    property bool noticeBad: false

    // The open note and what the editor's text is based on.
    property string currentId: ""
    property string baseVersion: ""
    property string baseText: ""
    property bool sending: false
    property string sentText: ""
    property bool applyingRemote: false
    property bool confirmRemove: false

    readonly property var current: (st.current && st.current.id === currentId) ? st.current : null
    readonly property var transport: st.transport || ({})

    function say(text, bad) {
        root.notice = text
        root.noticeBad = !!bad
        noticeTimer.restart()
    }
    Timer { id: noticeTimer; interval: 5000; onTriggered: root.notice = "" }

    function refresh() {
        if (root.pollBusy) { root.pollAgain = true; return }
        root.pollBusy = true
        var asked = root.currentId
        root.core("state", [asked], function (raw) {
            root.pollBusy = false
            var s = root.parse(raw)
            if (s && s.ok) {
                root.coreSeen = true
                root.missedPolls = 0
                root.st = s
                if (asked === root.currentId) root.mergeRemote(s.current)
            } else {
                root.missedPolls++
            }
            if (root.pollAgain) { root.pollAgain = false; root.refresh() }
        })
    }
    Timer { interval: 1000; running: true; repeat: true; onTriggered: root.refresh() }
    // A lost reply must not stop the poll forever.
    Timer { interval: 45000; running: root.pollBusy; onTriggered: root.pollBusy = false }
    Component.onCompleted: refresh()

    // Show other people's changes, unless the user has typed something not yet sent.
    function mergeRemote(c) {
        if (!c || c.id !== root.currentId) return
        if (root.sending || debounce.running) return
        if (editor.text !== root.baseText) return
        if (root.baseVersion !== "" && Number(c.version) <= Number(root.baseVersion)) return
        root.setEditor(c.text)
        root.baseText = c.text
        root.baseVersion = c.version
    }

    // Replace the editor text, keeping the cursor where it was relative to the text around it.
    function setEditor(t) {
        if (editor.text === t) return
        var old = editor.text
        var cur = editor.cursorPosition
        var p = 0
        while (p < old.length && p < t.length && old.charAt(p) === t.charAt(p)) p++
        var next = cur <= p ? cur : Math.max(p, cur + (t.length - old.length))
        root.applyingRemote = true
        editor.text = t
        root.applyingRemote = false
        editor.cursorPosition = Math.max(0, Math.min(next, t.length))
    }

    Timer { id: debounce; interval: 300; onTriggered: root.sendEdit() }

    function sendEdit() {
        if (root.currentId === "") return
        if (root.sending) { debounce.restart(); return }
        var text = editor.text
        if (text === root.baseText) return
        var id = root.currentId
        root.sending = true
        root.sentText = text
        root.core("set_text", [id, root.baseVersion, text], function (raw) {
            root.sending = false
            var r = root.parse(raw)
            if (root.currentId !== id) return
            if (!r) { root.say("Saving failed - is reflection_core loaded?", true); debounce.restart(); return }
            if (!r.ok) { root.say(r.error || "Saving failed", true); return }
            var c = r.current
            if (editor.text === root.sentText) {
                // Nothing typed meanwhile: show the merged text.
                root.setEditor(c.text)
                root.baseText = c.text
                root.baseVersion = c.version
            } else {
                // The user kept typing: their text builds on what was sent.
                root.baseText = root.sentText
                root.baseVersion = c.editVersion
                debounce.restart()
            }
        })
    }

    function openDoc(id, detail) {
        if (debounce.running) { debounce.stop(); root.sendEdit() }
        root.currentId = id
        root.confirmRemove = false
        var c = detail
        root.applyingRemote = true
        editor.text = c ? c.text : ""
        root.applyingRemote = false
        root.baseText = c ? c.text : ""
        root.baseVersion = c ? c.version : ""
        root.refresh()
    }

    function selectDoc(id) {
        if (id === root.currentId) return
        root.core("state", [id], function (raw) {
            var s = root.parse(raw)
            if (s && s.ok && s.current) { root.st = s; root.openDoc(id, s.current) }
            else root.say("Could not open the note", true)
        })
    }

    property bool creating: false
    function createDoc() {
        if (root.creating) return
        root.creating = true
        root.core("create_document", [newTitle.text], function (raw) {
            root.creating = false
            var r = root.parse(raw)
            if (!r) { root.say("Request failed - is reflection_core loaded?", true); return }
            if (!r.ok) { root.say(r.error || "Could not create the note", true); return }
            newTitle.text = ""
            root.openDoc(r.id, r.current)
            root.say("Note created")
            editor.forceActiveFocus()
            editor.cursorPosition = editor.text.length
        })
    }

    property bool joining: false
    function joinDoc() {
        if (root.joining) return
        var id = joinField.text.trim()
        if (id === "") { root.say("Paste the id of a shared note first", true); return }
        root.joining = true
        root.core("join_document", [id], function (raw) {
            root.joining = false
            var r = root.parse(raw)
            if (!r) { root.say("Request failed - is reflection_core loaded?", true); return }
            if (!r.ok) { root.say(r.error || "Could not open the note", true); return }
            joinField.text = ""
            root.openDoc(r.id, r.current)
            root.say("Joined. The content arrives from whoever is online.")
        })
    }

    function removeDoc() {
        if (!root.confirmRemove) { root.confirmRemove = true; return }
        var id = root.currentId
        root.core("remove_document", [id], function (raw) {
            var r = root.parse(raw)
            if (!r || !r.ok) { root.say((r && r.error) || "Could not remove the note", true); return }
            root.currentId = ""
            root.confirmRemove = false
            root.applyingRemote = true
            editor.text = ""
            root.applyingRemote = false
            root.say("Note removed from this device")
            root.refresh()
        })
    }

    function copy(text) {
        clip.text = text
        clip.selectAll()
        clip.copy()
        root.say("Copied the note id. Paste it into Reflection on another device.")
    }
    TextEdit { id: clip; visible: false }

    function peersText() {
        var m = root.transport.metrics
        if (m && typeof m === "object") {
            var n = m.peers !== undefined ? m.peers : (m.delivery && m.delivery.peers !== undefined ? m.delivery.peers : undefined)
            if (n !== undefined) return " - " + n + " peers"
        }
        return ""
    }
    function statusText() {
        if (!root.coreSeen) return root.missedPolls > 3 ? "reflection_core is not answering" : "Starting..."
        var node = String(root.st.node || "")
        if (node.indexOf("error") === 0) return "Engine " + node
        var t = root.transport.status || "starting"
        return "Logos: " + t + root.peersText()
    }
    function statusGood() { return !!root.transport.connected && String(root.st.node) === "ready" }

    component RField: TextField {
        Layout.fillWidth: true
        color: root.cText
        placeholderTextColor: root.cFaint
        font.pixelSize: 13
        selectByMouse: true
        background: Rectangle { radius: 6; color: root.cBg; border.width: 1; border.color: parent.activeFocus ? root.cAccent : root.cLine }
    }
    component RLabel: Text { color: root.cSub; font.pixelSize: 12; textFormat: Text.PlainText; elide: Text.ElideRight }

    Rectangle { anchors.fill: parent; color: root.cBg }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        // ── sidebar
        Rectangle {
            Layout.fillHeight: true
            Layout.preferredWidth: 270
            color: root.cSurface

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 12
                spacing: 8

                Text { text: "Reflection"; color: root.cText; font.pixelSize: 20; font.bold: true; textFormat: Text.PlainText }
                RLabel { text: "Notes you write together"; Layout.fillWidth: true }

                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    RField { id: newTitle; placeholderText: "New note title"; onAccepted: root.createDoc() }
                    LogosButton { text: root.creating ? "..." : "New"; enabled: !root.creating; onClicked: root.createDoc() }
                }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    RField { id: joinField; placeholderText: "Paste a note id to join"; onAccepted: root.joinDoc() }
                    LogosButton { text: root.joining ? "..." : "Join"; enabled: !root.joining; onClicked: root.joinDoc() }
                }

                Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: root.cLine }

                ListView {
                    id: docList
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    spacing: 2
                    model: root.st.docs || []
                    delegate: Rectangle {
                        required property var modelData
                        width: docList.width
                        height: 44
                        radius: 6
                        color: modelData.id === root.currentId ? root.cRaised : (hover.containsMouse ? Qt.darker(root.cRaised, 1.15) : "transparent")
                        ColumnLayout {
                            anchors.fill: parent
                            anchors.leftMargin: 10
                            anchors.rightMargin: 10
                            spacing: 1
                            Text {
                                Layout.fillWidth: true
                                text: modelData.name
                                color: root.cText
                                font.pixelSize: 14
                                elide: Text.ElideRight
                                textFormat: Text.PlainText
                            }
                            RLabel {
                                Layout.fillWidth: true
                                font.pixelSize: 11
                                color: root.cFaint
                                text: (modelData.online > 0 ? (modelData.online + " online") : "nobody else online")
                                      + (modelData.subscribed ? "" : " - connecting")
                            }
                        }
                        MouseArea { id: hover; anchors.fill: parent; hoverEnabled: true; onClicked: root.selectDoc(modelData.id) }
                    }
                    RLabel {
                        anchors.centerIn: parent
                        visible: docList.count === 0
                        text: root.coreSeen ? "No notes yet" : ""
                    }
                }

                Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: root.cLine }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    Rectangle { width: 8; height: 8; radius: 4; color: root.statusGood() ? root.cOk : (root.coreSeen ? root.cAccent : root.cBad) }
                    RLabel { Layout.fillWidth: true; text: root.statusText() }
                }
                RLabel {
                    Layout.fillWidth: true
                    visible: !!root.transport.error
                    color: root.cFaint
                    font.pixelSize: 11
                    text: root.transport.error ? ("Last transport error: " + root.transport.error) : ""
                    wrapMode: Text.Wrap
                    maximumLineCount: 3
                }
                RLabel {
                    Layout.fillWidth: true
                    color: root.cFaint
                    font.pixelSize: 10
                    text: "view " + root.viewVersion + (root.st.me ? (" - you are " + String(root.st.me).substring(0, 8)) : "")
                }
            }
        }

        Rectangle { Layout.fillHeight: true; Layout.preferredWidth: 1; color: root.cLine }

        // ── the open note
        ColumnLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.margins: 16
            spacing: 10

            RowLayout {
                Layout.fillWidth: true
                visible: root.currentId !== ""
                spacing: 8
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 2
                    Text {
                        Layout.fillWidth: true
                        text: root.current ? root.current.name : ""
                        color: root.cText
                        font.pixelSize: 18
                        font.bold: true
                        elide: Text.ElideRight
                        textFormat: Text.PlainText
                    }
                    RLabel {
                        Layout.fillWidth: true
                        text: {
                            if (!root.current) return ""
                            var n = (root.current.online || []).length
                            var who = n === 0 ? "Only you here right now" : (n === 1 ? "1 other person here" : (n + " other people here"))
                            var sync = root.current.subscribed ? "" : " - not connected yet"
                            var save = root.sending || debounce.running ? " - saving..." : ""
                            return who + sync + save
                        }
                    }
                }
                LogosButton { text: "Copy id"; onClicked: root.copy(root.currentId) }
                LogosButton { text: root.confirmRemove ? "Click again to remove" : "Remove"; onClicked: root.removeDoc() }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.fillHeight: true
                radius: 8
                color: root.cSurface
                border.width: 1
                border.color: editor.activeFocus ? root.cAccent : root.cLine
                visible: root.currentId !== ""

                ScrollView {
                    id: scroller
                    anchors.fill: parent
                    anchors.margins: 1
                    clip: true
                    TextArea {
                        id: editor
                        wrapMode: TextEdit.Wrap
                        color: root.cText
                        selectByMouse: true
                        font.pixelSize: 15
                        padding: 14
                        placeholderText: "Write here. The first line is the note's title."
                        placeholderTextColor: root.cFaint
                        textFormat: TextEdit.PlainText
                        background: null
                        onTextChanged: if (!root.applyingRemote && root.currentId !== "") debounce.restart()
                    }
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                visible: root.currentId === ""
                spacing: 8
                Item { Layout.fillHeight: true }
                Text {
                    Layout.alignment: Qt.AlignHCenter
                    text: "Create a note, or join one with an id someone shared."
                    color: root.cSub
                    font.pixelSize: 15
                    textFormat: Text.PlainText
                }
                RLabel {
                    Layout.alignment: Qt.AlignHCenter
                    text: "Notes sync over Logos Delivery with everyone who has the id."
                }
                Item { Layout.fillHeight: true }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: noticeText.implicitHeight + 12
                visible: root.notice !== ""
                radius: 6
                color: root.noticeBad ? Qt.darker(root.cBad, 2.2) : root.cRaised
                Text {
                    id: noticeText
                    anchors.fill: parent
                    anchors.margins: 6
                    text: root.notice
                    color: root.noticeBad ? root.cBad : root.cText
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
            }
        }
    }
}
