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
    readonly property color cCode: tok(pal.success, "#7fc79a")
    readonly property color cLink: "#8ab4f8"
    readonly property color cEmph: "#e9b97a"
    readonly property string viewVersion: "0.6.0"

    // Build switches. `debugTools` shows the message-stats button (how many Logos messages this
    // device sends, mapped to the RLN budget); set it to false for a build without it.
    readonly property bool debugTools: true
    // Colour Markdown syntax while editing (the text stays plain; nothing is rendered).
    readonly property bool markdownColours: true

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
        var seq = root.settingsSeq
        root.core("state", [asked], function (raw) {
            root.pollBusy = false
            var s = root.parse(raw)
            if (s && s.ok) {
                root.coreSeen = true
                root.missedPolls = 0
                // A reply to a request made before a settings change has the old settings.
                if (seq !== root.settingsSeq) s.settings = root.st.settings
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
                cursorDebounce.restart()
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
        var seq = root.settingsSeq
        root.core("state", [id], function (raw) {
            var s = root.parse(raw)
            if (s && s.ok && seq !== root.settingsSeq) s.settings = root.st.settings
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
        if (root.networks !== "logos") return root.networksText()
        return "Logos: " + t + root.peersText()
    }
    function statusGood() {
        if (String(root.st.node) !== "ready") return false
        var ns = root.st.networkStatus || {}
        if (root.networks === "p2panda") return !!(ns.p2panda && ns.p2panda.connected)
        if (root.networks === "both") return !!root.transport.connected || !!(ns.p2panda && ns.p2panda.connected)
        return !!root.transport.connected
    }

    // ── message stats (debug)
    property bool showStats: false
    property var stats: null
    property string statsError: ""
    function refreshStats() {
        root.core("stats", [], function (raw) {
            var r = root.parse(raw)
            if (r && r.ok) { root.stats = r; root.statsError = "" }
            else root.statsError = (r && r.error) ? r.error : "This reflection_core has no stats (update it)"
        })
    }
    Timer { interval: 2000; running: root.showStats; repeat: true; triggeredOnStart: true; onTriggered: root.refreshStats() }
    readonly property var settings: st.settings || ({})
    property int settingsSeq: 0
    function setSettings(changes, done) {
        root.settingsSeq++
        root.core("set_settings", [JSON.stringify(changes)], function (raw) {
            var r = root.parse(raw)
            if (!r) { root.say("This reflection_core can't change settings (update it)", true); return }
            if (!r.ok) { root.say(r.error || "Could not change the setting", true); return }
            root.settingsSeq++
            root.st = Object.assign({}, root.st, { settings: r.settings })
            if (r.switching) root.say("Switching networks...")
            if (done) done()
            if (root.showStats) root.refreshStats()
        })
    }

    // ── cursors: ours goes to the core (and on to the others if shared) once the editor's
    // text is what the core has, so its offsets mean the same thing on both sides.
    Timer { id: cursorDebounce; interval: 300; onTriggered: root.sendCursor() }
    function sendCursor() {
        if (root.currentId === "" || !root.settings.shareCursor) return
        if (root.sending || debounce.running || editor.text !== root.baseText) { cursorDebounce.restart(); return }
        var pos = editor.cursorPosition
        var anchor = editor.selectionStart === editor.selectionEnd ? pos
                   : (pos === editor.selectionStart ? editor.selectionEnd : editor.selectionStart)
        root.core("set_cursor", [root.currentId, root.baseVersion, String(pos), String(anchor)], function () {})
    }
    readonly property var cursorColours: ["#5fb3f9", "#f97fb3", "#9be37a", "#f9c45f", "#b48cf9", "#5fe0d0"]
    function colourOf(key) {
        var h = 0
        for (var i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) % 9973
        return root.cursorColours[h % root.cursorColours.length]
    }

    function kb(bytes) { return bytes >= 1024 ? (Math.round(bytes / 102.4) / 10) + " KB" : bytes + " B" }
    function total(c) {
        if (!c) return 0
        return (c.ops || 0) + (c.snapshots || 0) + (c.heads || 0) + (c.repairs || 0) + (c.relayed || 0) + (c.presence || 0) + (c.cursors || 0)
    }
    function ago(s) { return s < 60 ? s + " s ago" : (s < 3600 ? Math.floor(s / 60) + " min ago" : Math.floor(s / 3600) + " h ago") }
    function clock(s) { return Math.floor(s / 60) + ":" + ("0" + (s % 60)).slice(-2) }

    // ── Markdown colours: the editor's own text is transparent, and a coloured copy of it is
    // drawn underneath with the same font and wrapping. Colour only: a bold or larger glyph
    // would move the text away from the editor's cursor.
    function esc(t) { return t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;") }
    function span(c, t) { return t === "" ? "" : "<span style=\"color:" + c + "\">" + t + "</span>" }
    function mdInline(raw) {
        var e = root.esc(raw)
        var re = /(`+)([^`]+?)\1|(\*\*|__)(\S(?:.*?\S)?)\3|(\*|_)(\S(?:.*?\S)?)\5|\[([^\]]+)\]\(([^)\s]+)\)/g
        return e.replace(re, function (m, tick, code, b, btxt, i, itxt, ltxt, lurl, offset, str) {
            if (tick) return root.span(root.cCode, m)
            if (b) return root.span(root.cFaint, b) + root.span(root.cEmph, btxt) + root.span(root.cFaint, b)
            if (i) {
                // snake_case and 2*3*4 are not emphasis.
                if (offset > 0 && /\w/.test(str.charAt(offset - 1))) return m
                return root.span(root.cFaint, i) + root.span(root.cEmph, itxt) + root.span(root.cFaint, i)
            }
            if (ltxt) return root.span(root.cFaint, "[") + root.span(root.cLink, ltxt) + root.span(root.cFaint, "](" + lurl + ")")
            return m
        })
    }
    function mdHtml(text) {
        var lines = text.split("\n")
        var out = []
        var fence = false
        for (var n = 0; n < lines.length; n++) {
            var l = lines[n]
            var m
            if (/^\s*(```|~~~)/.test(l)) { fence = !fence; out.push(root.span(root.cFaint, root.esc(l))); continue }
            if (fence) { out.push(root.span(root.cCode, root.esc(l))); continue }
            if ((m = /^(#{1,6})(\s.*|)$/.exec(l))) { out.push(root.span(root.cFaint, m[1]) + root.span(root.cAccent, root.esc(m[2]))); continue }
            if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(l)) { out.push(root.span(root.cFaint, root.esc(l))); continue }
            if ((m = /^(\s*)(>+)(.*)$/.exec(l))) { out.push(m[1] + root.span(root.cAccent, root.esc(m[2])) + root.span(root.cSub, root.mdInline(m[3]))); continue }
            if ((m = /^(\s*)([-*+]|\d+[.)])(\s+)(\[[ xX]\]\s)?(.*)$/.exec(l))) {
                out.push(m[1] + root.span(root.cAccent, root.esc(m[2])) + m[3]
                         + (m[4] ? root.span(/x/i.test(m[4]) ? root.cCode : root.cFaint, m[4]) : "")
                         + root.mdInline(m[5]))
                continue
            }
            out.push(root.mdInline(l))
        }
        return "<div style=\"white-space:pre-wrap\">" + out.join("\n") + "</div>"
    }
    readonly property bool mdOn: root.markdownColours && editor.text.length > 0 && editor.text.length < 30000
                                 && !editor.inputMethodComposing

    component RField: TextField {
        Layout.fillWidth: true
        color: root.cText
        placeholderTextColor: root.cFaint
        font.pixelSize: 13
        selectByMouse: true
        background: Rectangle { radius: 6; color: root.cBg; border.width: 1; border.color: parent.activeFocus ? root.cAccent : root.cLine }
    }
    // A pill-shaped switch.
    component RToggle: Rectangle {
        id: toggle
        property bool on: false
        property string label: ""
        signal toggled()
        Layout.preferredHeight: 34
        Layout.preferredWidth: toggleRow.implicitWidth + 20
        radius: 17
        color: toggleHover.containsMouse ? root.cRaised : "transparent"
        border.width: 1
        border.color: toggle.on ? root.cAccent : root.cLine
        RowLayout {
            id: toggleRow
            anchors.centerIn: parent
            spacing: 6
            Rectangle {
                width: 28; height: 16; radius: 8
                color: toggle.on ? root.cAccent : root.cBg
                border.width: 1; border.color: root.cLine
                Rectangle { width: 12; height: 12; radius: 6; y: 2; x: toggle.on ? 14 : 2; color: root.cText }
            }
            Text { text: toggle.label; color: root.cText; font.pixelSize: 12; textFormat: Text.PlainText }
        }
        MouseArea { id: toggleHover; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: toggle.toggled() }
    }

    // The networks notes sync over (reflection_core 0.6.0 `networks`): Logos, p2panda or both.
    readonly property string networks: st.networks || st.syncMode || "logos"
    readonly property string chosenNetworks: (root.settings && (root.settings.networks || root.settings.syncMode)) || "logos"
    readonly property bool p2pandaMode: root.networks === "p2panda"
    readonly property var networkChoices: [["logos", "Logos"], ["p2panda", "p2panda"], ["both", "Both"]]
    function networksText() {
        var ns = root.st.networkStatus || {}
        function one(name, s) {
            if (!s) return name + ": ?"
            return name + ": " + (s.connected ? (s.peers + " peers") : (s.open === false && s.error ? "off" : "connecting"))
        }
        if (root.networks === "both") return one("Logos", ns.logos) + " · " + one("p2panda", ns.p2panda)
        if (root.networks === "p2panda") return one("p2panda", ns.p2panda)
        return ""
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
            Layout.minimumWidth: 270
            Layout.maximumWidth: 270
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
                    LogosButton { text: root.creating ? "..." : "New"; Layout.preferredWidth: 64; Layout.preferredHeight: 34; enabled: !root.creating; onClicked: root.createDoc() }
                }
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    RField { id: joinField; placeholderText: "Paste a note id to join"; onAccepted: root.joinDoc() }
                    LogosButton { text: root.joining ? "..." : "Join"; Layout.preferredWidth: 64; Layout.preferredHeight: 34; enabled: !root.joining; onClicked: root.joinDoc() }
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
                RowLayout {
                    Layout.fillWidth: true
                    spacing: 6
                    RLabel {
                        Layout.fillWidth: true
                        color: root.cFaint
                        font.pixelSize: 10
                        text: "view " + root.viewVersion + (root.st.me ? (" - you are " + String(root.st.me).substring(0, 8)) : "")
                    }
                    Text {
                        id: statsLink
                        visible: root.debugTools
                        text: root.showStats ? "Hide stats" : "Message stats"
                        color: statsHover.containsMouse ? root.cAccent : root.cSub
                        font.pixelSize: 10
                        font.underline: true
                        textFormat: Text.PlainText
                        MouseArea { id: statsHover; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: root.showStats = !root.showStats }
                    }
                }
            }
        }

        Rectangle { Layout.fillHeight: true; Layout.preferredWidth: 1; color: root.cLine }

        // ── the open note
        ColumnLayout {
            Layout.fillWidth: true
            Layout.minimumWidth: 300
            Layout.maximumWidth: 100000
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
                LogosButton { text: "Copy id"; Layout.preferredWidth: 96; Layout.preferredHeight: 34; onClicked: root.copy(root.currentId) }
                LogosButton { text: root.confirmRemove ? "Really remove?" : "Remove"; Layout.preferredWidth: 120; Layout.preferredHeight: 34; onClicked: root.removeDoc() }
            }

            // note switches: cursor sharing, and the networks notes sync over
            RowLayout {
                Layout.fillWidth: true
                visible: root.currentId !== ""
                spacing: 8
                Rectangle {
                    id: shareToggle
                    Layout.preferredHeight: 34
                    Layout.preferredWidth: shareRow.implicitWidth + 20
                    radius: 17
                    color: shareHover.containsMouse ? root.cRaised : "transparent"
                    border.width: 1
                    border.color: root.settings.shareCursor ? root.cAccent : root.cLine
                    RowLayout {
                        id: shareRow
                        anchors.centerIn: parent
                        spacing: 6
                        Rectangle {
                            width: 28; height: 16; radius: 8
                            color: root.settings.shareCursor ? root.cAccent : root.cBg
                            border.width: 1; border.color: root.cLine
                            Rectangle {
                                width: 12; height: 12; radius: 6; y: 2
                                x: root.settings.shareCursor ? 14 : 2
                                color: root.cText
                            }
                        }
                        Text { text: "Share my cursor"; color: root.cText; font.pixelSize: 12; textFormat: Text.PlainText }
                    }
                    MouseArea {
                        id: shareHover
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.setSettings({ shareCursor: !root.settings.shareCursor }, function () { cursorDebounce.restart() })
                    }
                }
                RLabel { text: "Networks"; visible: !!root.st.p2pandaAvailable }
                Repeater {
                    model: !!root.st.p2pandaAvailable ? root.networkChoices : []
                    delegate: LogosButton {
                        required property var modelData
                        text: ((root.chosenNetworks === modelData[0]) ? "● " : "") + modelData[1]
                        Layout.preferredHeight: 30
                        Layout.preferredWidth: 84
                        Layout.fillWidth: false
                        onClicked: root.setSettings({ networks: modelData[0] })
                    }
                }
                Item { Layout.fillWidth: true }
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
                        color: root.mdOn ? "transparent" : root.cText
                        selectedTextColor: root.cText
                        selectionColor: Qt.rgba(root.cAccent.r, root.cAccent.g, root.cAccent.b, 0.35)
                        selectByMouse: true
                        font.pixelSize: 15
                        padding: 14
                        placeholderText: "Write here. The first line is the note's title."
                        placeholderTextColor: root.cFaint
                        textFormat: TextEdit.PlainText
                        background: null
                        onTextChanged: if (!root.applyingRemote && root.currentId !== "") debounce.restart()
                        onCursorPositionChanged: if (root.currentId !== "") cursorDebounce.restart()
                        onSelectionStartChanged: if (root.currentId !== "") cursorDebounce.restart()

                        // Our caret. The default one is drawn in the text colour, which is
                        // transparent while Markdown is coloured.
                        cursorDelegate: Rectangle {
                            id: caret
                            width: 2
                            color: root.cAccent
                            visible: editor.activeFocus && blink.on
                            Timer {
                                id: blink
                                property bool on: true
                                interval: 530
                                repeat: true
                                running: editor.activeFocus
                                onTriggered: on = !on
                                onRunningChanged: on = true
                            }
                            Connections {
                                target: editor
                                function onCursorPositionChanged() { blink.on = true; blink.restart() }
                            }
                        }

                        // Other people's cursors (from the core, in the current text).
                        Repeater {
                            model: (root.current && root.current.cursors) ? root.current.cursors : []
                            delegate: Item {
                                id: remote
                                required property var modelData
                                readonly property int pos: Math.max(0, Math.min(modelData.pos, editor.length))
                                readonly property rect r: { editor.text; editor.width; return editor.positionToRectangle(pos) }
                                readonly property color tint: root.colourOf(modelData.author)
                                x: r.x
                                y: r.y
                                z: 2
                                Rectangle { width: 2; height: remote.r.height; color: remote.tint }
                                Rectangle {
                                    y: -height
                                    width: tag.implicitWidth + 6
                                    height: tag.implicitHeight + 2
                                    radius: 3
                                    color: remote.tint
                                    Text {
                                        id: tag
                                        anchors.centerIn: parent
                                        text: String(remote.modelData.author).substring(0, 6)
                                        color: "#111"
                                        font.pixelSize: 9
                                        textFormat: Text.PlainText
                                    }
                                }
                            }
                        }

                        Text {
                            id: mdLayer
                            z: -1
                            visible: root.mdOn
                            x: editor.leftPadding
                            y: editor.topPadding
                            width: editor.width - editor.leftPadding - editor.rightPadding
                            font: editor.font
                            wrapMode: Text.Wrap
                            color: root.cText
                            textFormat: Text.RichText
                            text: root.mdOn ? root.mdHtml(editor.text) : ""
                        }
                    }
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                visible: root.currentId === ""
                Layout.maximumWidth: 100000
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

        // ── message stats (debug): what this device sends, in RLN terms
        Rectangle { Layout.fillHeight: true; Layout.preferredWidth: 1; color: root.cLine; visible: root.showStats }
        Rectangle {
            id: statsPanel
            visible: root.showStats
            Layout.fillHeight: true
            Layout.preferredWidth: 330
            Layout.minimumWidth: 330
            Layout.maximumWidth: 330
            color: root.cSurface
            readonly property var s: root.stats ? root.stats.stats : null
            readonly property bool hasBudget: !!(root.stats && root.stats.budget)
            readonly property int budget: statsPanel.hasBudget ? root.stats.budget : 0
            readonly property int used: root.total(s ? s.sent_this_epoch : null)

            ScrollView {
                anchors.fill: parent
                anchors.margins: 12
                clip: true
                contentWidth: availableWidth

                ColumnLayout {
                    width: statsPanel.width - 24
                    spacing: 6

                    RowLayout {
                        Layout.fillWidth: true
                        Text { Layout.fillWidth: true; text: "Message stats"; color: root.cText; font.pixelSize: 16; font.bold: true; textFormat: Text.PlainText }
                        Text {
                            text: "Close"; color: root.cSub; font.pixelSize: 11; font.underline: true; textFormat: Text.PlainText
                            MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: root.showStats = false }
                        }
                    }
                    RLabel {
                        Layout.fillWidth: true
                        wrapMode: Text.Wrap
                        elide: Text.ElideNone
                        font.pixelSize: 11
                        text: "Logos Delivery will meter what a node publishes (RLN): a membership allows a number of messages per 10-minute epoch, shared by every app on the node. Receiving is free. Notes that aren't open send nothing; the open one announces itself when opened and every 2 minutes, edits are merged, and your cursor is sent only if you share it."
                    }
                    RLabel { visible: root.statsError !== ""; Layout.fillWidth: true; wrapMode: Text.Wrap; color: root.cBad; text: root.statsError }
                    RLabel { visible: root.stats !== null && !statsPanel.s; Layout.fillWidth: true; wrapMode: Text.Wrap; text: "No counters yet (the engine is starting)." }

                    // this epoch against the budget
                    Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: root.cLine; visible: !!statsPanel.s }
                    ColumnLayout {
                        visible: !!statsPanel.s
                        Layout.fillWidth: true
                        spacing: 4
                        RowLayout {
                            Layout.fillWidth: true
                            Text { text: "This epoch"; color: root.cText; font.pixelSize: 13; font.bold: true; textFormat: Text.PlainText }
                            Item { Layout.fillWidth: true }
                            RLabel {
                                text: statsPanel.s ? (root.clock(statsPanel.s.epoch_seconds - statsPanel.s.epoch_elapsed) + " left") : ""
                            }
                        }
                        Text {
                            text: statsPanel.used + (statsPanel.hasBudget ? (" of " + statsPanel.budget) : "") + " messages"
                            color: !statsPanel.hasBudget ? root.cText : (statsPanel.used * 10 >= statsPanel.budget * 8 ? root.cBad : (statsPanel.used * 2 >= statsPanel.budget ? root.cAccent : root.cText))
                            font.pixelSize: 22
                            textFormat: Text.PlainText
                        }
                        Rectangle {
                            visible: statsPanel.hasBudget
                            Layout.fillWidth: true
                            Layout.preferredHeight: 6
                            radius: 3
                            color: root.cBg
                            Rectangle {
                                width: parent.width * Math.min(1, statsPanel.used / Math.max(1, statsPanel.budget))
                                height: parent.height
                                radius: 3
                                color: statsPanel.used * 10 >= statsPanel.budget * 8 ? root.cBad : (statsPanel.used * 2 >= statsPanel.budget ? root.cAccent : root.cOk)
                            }
                        }
                        RLabel {
                            Layout.fillWidth: true
                            wrapMode: Text.Wrap
                            elide: Text.ElideNone
                            font.pixelSize: 11
                            text: {
                                if (!root.stats) return ""
                                var t = "Edits now go out at most every " + (Math.round(root.stats.publishInterval * 10) / 10) + " s"
                                t += root.stats.cursorInterval === null ? ", your cursor not at all (budget short)" : (", your cursor every " + (Math.round(root.stats.cursorInterval * 10) / 10) + " s if shared")
                                t += "; a snapshot follows " + root.stats.snapshotQuiet + " s after you stop typing."
                                var b = statsPanel.s ? statsPanel.s.sent_this_epoch.blocked : 0
                                if (b > 0) t += " " + b + " messages were held back this epoch (over the budget)."
                                return t
                            }
                        }
                        RLabel {
                            Layout.fillWidth: true
                            font.pixelSize: 11
                            text: statsPanel.s ? ("Last epoch: " + root.total(statsPanel.s.sent_last_epoch) + " messages") : ""
                        }

                        // pretend budget, to try out RLN tiers
                        RLabel { Layout.fillWidth: true; Layout.topMargin: 4; text: "Pretend RLN limit (messages per epoch)"; color: root.cText }
                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 6
                            RField {
                                id: budgetField
                                placeholderText: "no limit"
                                text: root.settings.budget ? String(root.settings.budget) : ""
                                inputMethodHints: Qt.ImhDigitsOnly
                                validator: RegularExpressionValidator { regularExpression: /[0-9]{0,6}/ }
                                onAccepted: root.setSettings({ budget: budgetField.text.trim() === "" ? null : budgetField.text.trim() })
                            }
                            LogosButton {
                                text: "Set"
                                Layout.preferredWidth: 56
                                Layout.preferredHeight: 30
                                onClicked: root.setSettings({ budget: budgetField.text.trim() === "" ? null : budgetField.text.trim() })
                            }
                        }
                        RLabel {
                            Layout.fillWidth: true
                            wrapMode: Text.Wrap
                            elide: Text.ElideNone
                            font.pixelSize: 10
                            color: root.cFaint
                            text: "Not enforced by the network yet. Past the limit nothing more is sent this epoch (as RLN would drop it); edits slow down from half of it. Empty: no limit."
                        }

                        // which networks Reflection syncs over (chosen next to the note)
                        RLabel {
                            Layout.fillWidth: true
                            Layout.topMargin: 8
                            wrapMode: Text.Wrap
                            elide: Text.ElideNone
                            font.pixelSize: 10
                            color: root.cFaint
                            text: {
                                var names = { logos: "Logos Delivery", p2panda: "p2panda (the network of upstream Reflection)", both: "Logos Delivery and p2panda" }
                                var t = "Syncing over " + (names[root.networks] || root.networks) + "."
                                if (root.chosenNetworks !== root.networks) t += " Switching to " + (names[root.chosenNetworks] || root.chosenNetworks) + "..."
                                if (root.networks === "both") t += " Every edit goes out on both, and what arrives on one is passed on to the other. Only Logos messages count against the budget."
                                return t
                            }
                        }
                    }

                    // breakdown
                    Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: root.cLine; visible: !!statsPanel.s }
                    GridLayout {
                        visible: !!statsPanel.s
                        Layout.fillWidth: true
                        columns: 3
                        columnSpacing: 10
                        rowSpacing: 2
                        RLabel { text: "Sent, by kind"; color: root.cText; font.bold: true; Layout.fillWidth: true }
                        RLabel { text: "epoch"; horizontalAlignment: Text.AlignRight; Layout.preferredWidth: 50 }
                        RLabel { text: "total"; horizontalAlignment: Text.AlignRight; Layout.preferredWidth: 50 }
                        Repeater {
                            model: [
                                ["ops", "Edits (merged)"], ["snapshots", "Snapshots"], ["heads", "Catch-up requests"],
                                ["repairs", "Catch-up answers"], ["presence", "Presence"], ["cursors", "Cursors"], ["relayed", "Relayed (bridge)"],
                                ["blocked", "Held back (over limit)"]
                            ]
                            delegate: RLabel {
                                required property var modelData
                                required property int index
                                Layout.row: index + 1
                                Layout.column: 0
                                Layout.fillWidth: true
                                text: modelData[1]
                            }
                        }
                        Repeater {
                            model: ["ops", "snapshots", "heads", "repairs", "presence", "cursors", "relayed", "blocked"]
                            delegate: RLabel {
                                required property var modelData
                                required property int index
                                Layout.row: index + 1
                                Layout.column: 1
                                Layout.preferredWidth: 50
                                horizontalAlignment: Text.AlignRight
                                color: root.cText
                                text: statsPanel.s ? String(statsPanel.s.sent_this_epoch[modelData] || 0) : ""
                            }
                        }
                        Repeater {
                            model: ["ops", "snapshots", "heads", "repairs", "presence", "cursors", "relayed", "blocked"]
                            delegate: RLabel {
                                required property var modelData
                                required property int index
                                Layout.row: index + 1
                                Layout.column: 2
                                Layout.preferredWidth: 50
                                horizontalAlignment: Text.AlignRight
                                text: statsPanel.s ? String(statsPanel.s.sent[modelData] || 0) : ""
                            }
                        }
                    }
                    RLabel {
                        visible: !!statsPanel.s
                        Layout.fillWidth: true
                        wrapMode: Text.Wrap
                        elide: Text.ElideNone
                        font.pixelSize: 11
                        text: statsPanel.s ? ("Since start (" + root.clock(statsPanel.s.uptime) + "): sent " + root.total(statsPanel.s.sent) + " (" + root.kb(statsPanel.s.sent.bytes)
                                              + "), received " + root.total(statsPanel.s.received) + " (" + root.kb(statsPanel.s.received.bytes) + ")") : ""
                    }
                    RLabel {
                        Layout.fillWidth: true
                        font.pixelSize: 11
                        text: "Delivery node: " + (root.transport.status || "?") + root.peersText()
                    }

                    // per note
                    Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: root.cLine; visible: !!statsPanel.s }
                    Text { visible: !!statsPanel.s; text: "Per note"; color: root.cText; font.pixelSize: 13; font.bold: true; textFormat: Text.PlainText }
                    Repeater {
                        model: statsPanel.s ? statsPanel.s.topics : []
                        delegate: Rectangle {
                            required property var modelData
                            Layout.fillWidth: true
                            Layout.preferredHeight: noteCol.implicitHeight + 12
                            radius: 6
                            color: modelData.active ? root.cRaised : "transparent"
                            border.width: 1
                            border.color: root.cLine
                            ColumnLayout {
                                id: noteCol
                                anchors.fill: parent
                                anchors.margins: 6
                                spacing: 1
                                RowLayout {
                                    Layout.fillWidth: true
                                    Text {
                                        Layout.fillWidth: true
                                        text: (root.stats && root.stats.names[modelData.topic]) || modelData.topic.substring(0, 12)
                                        color: root.cText
                                        font.pixelSize: 12
                                        elide: Text.ElideRight
                                        textFormat: Text.PlainText
                                    }
                                    RLabel { text: modelData.active ? "open" : "background"; color: modelData.active ? root.cOk : root.cFaint; font.pixelSize: 10 }
                                }
                                RLabel {
                                    Layout.fillWidth: true
                                    font.pixelSize: 11
                                    text: "sent " + root.total(modelData.sent_this_epoch) + " this epoch, " + root.total(modelData.sent) + " total - received " + root.total(modelData.received)
                                }
                                RLabel {
                                    Layout.fillWidth: true
                                    font.pixelSize: 11
                                    color: root.cFaint
                                    wrapMode: Text.Wrap
                                    elide: Text.ElideNone
                                    text: {
                                        var p = modelData.peers || []
                                        if (p.length === 0) return "no other devices heard from"
                                        var parts = []
                                        for (var i = 0; i < p.length && i < 4; i++) parts.push(p[i].key.substring(0, 8) + " " + root.ago(p[i].seconds_ago))
                                        return p.length + (p.length === 1 ? " device: " : " devices: ") + parts.join(", ") + (p.length > 4 ? ", ..." : "")
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
