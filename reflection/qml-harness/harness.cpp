// Offscreen render + behaviour check of the Reflection view with a fake reflection_core.
// Prints CHECK-OK / CHECK-FAIL lines; exit status = number of failures.
#include <QGuiApplication>
#include <QQuickView>
#include <QQmlContext>
#include <QQmlEngine>
#include <QQmlExpression>
#include <QQuickItem>
#include <QJSValue>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonArray>
#include <QTimer>
#include <QElapsedTimer>
#include <memory>
#include <QImage>
#include <cstdio>

class FakeCore : public QObject {
    Q_OBJECT
public:
    struct Doc { QString name, text; int version = 1; int editVersion = 1; };
    QMap<QString, Doc> docs;
    int calls = 0;
    int delayMs = 0;
    bool shareCursor = false;
    QString syncMode = "logos";
    bool bridgeRequested = false;
    QString lastCursor;
    QJsonArray remoteCursors;
    Q_INVOKABLE QString callModule(const QString &mod, const QString &method, const QVariant &args);
    Q_INVOKABLE void callModuleAsync(const QString &mod, const QString &method, const QVariant &args, const QJSValue &cb, int timeoutMs);
    Q_INVOKABLE void remoteSet(const QString &id, const QString &text) { auto &d = docs[id]; d.text = text; d.version++; d.name = text.section('\n', 0, 0); }
    Q_INVOKABLE QString coreText(const QString &id) { return docs.value(id).text; }
    Q_INVOKABLE void setDelay(int ms) { delayMs = ms; }
};

static QJsonArray core_cursors;
static QJsonObject detail(const QString &id, const FakeCore::Doc &d) {
    return QJsonObject{{"id", id}, {"name", d.name.isEmpty() ? "Untitled" : d.name}, {"text", d.text},
                       {"version", QString::number(d.version)}, {"online", QJsonArray{"ab12"}}, {"subscribed", true},
                       {"cursors", core_cursors}};
}

QString FakeCore::callModule(const QString &mod, const QString &method, const QVariant &v) {
    calls++;
    QVariantList a = v.toList();
    auto arg = [&](int i) { return i < a.size() ? a[i].toString() : QString(); };
    QJsonObject r{{"ok", true}};
    if (mod == "p2panda_core") {
        // Enough of p2panda_core for its view (p2panda_ui/Main.qml).
        QString net = "d993c823dc344bd50c7bb24037071b54d0e0575ade66059550ca3bd5a6554681";
        if (method == "status")
            return QJsonDocument(QJsonObject{{"ok", true}, {"apiVersion", 1},
                {"networks", QJsonArray{QJsonObject{{"network", net}, {"app", "reflection"}, {"author", "1a94a5ea6f111277577b20efdacc5129fe93450daec6500a63f3df0469d0dea5"}, {"bridged", 1},
                    {"logos", QJsonObject{{"sent_this_epoch", QJsonObject{{"relayed", 4}, {"heads", 1}}}}}}}},
                {"subscriptions", QJsonArray{QJsonObject{{"network", net}}, QJsonObject{{"network", net}}}}}).toJson(QJsonDocument::Compact);
        if (method == "bridges")
            return QJsonDocument(QJsonObject{{"ok", true}, {"bridges", QJsonArray{
                QJsonObject{{"network", net}, {"topic", "b6505f981ea7a14a16e7ca747cc78814a9064a9e7425f88d2bddb2ffe5378c16"}, {"app", "reflection"}, {"label", "Plan for May"}, {"state", "pending"}},
                QJsonObject{{"network", net}, {"topic", "2729ea81e63ec653644a541863725571d63667695c4ab19941dc749681ccaa0c"}, {"app", "reflection"}, {"label", "Shopping"}, {"state", "on"}}}}}).toJson(QJsonDocument::Compact);
        if (method == "bridge_confirm") return QJsonDocument(QJsonObject{{"ok", true}}).toJson(QJsonDocument::Compact);
        return "";
    }
    if (mod != "reflection_core") return "";
    if (method == "state") {
        QJsonArray list;
        for (auto it = docs.begin(); it != docs.end(); ++it)
            list.append(QJsonObject{{"id", it.key()}, {"name", it->name.isEmpty() ? "Untitled" : it->name}, {"online", 1}, {"updated", 0}, {"subscribed", true}});
        r["docs"] = list; r["node"] = "ready"; r["me"] = "cafe1234";
        r["transport"] = QJsonObject{{"status", "Connected"}, {"connected", true}, {"metrics", QJsonObject{{"peers", 6}}}};
        r["current"] = docs.contains(arg(0)) ? QJsonValue(detail(arg(0), docs[arg(0)])) : QJsonValue();
        r["settings"] = QJsonObject{{"budget", QJsonValue()}, {"shareCursor", shareCursor}, {"networks", syncMode}, {"syncMode", syncMode}};
        r["networks"] = syncMode; r["syncMode"] = syncMode; r["p2pandaAvailable"] = true;
        r["networkStatus"] = QJsonObject{
            {"logos", QJsonObject{{"used", syncMode != "p2panda"}, {"connected", true}, {"peers", 7}, {"status", "Connected"}}},
            {"p2panda", QJsonObject{{"used", syncMode != "logos"}, {"open", syncMode != "logos"}, {"connected", syncMode != "logos"}, {"peers", 2}}}};
    } else if (method == "set_settings") {
        QJsonObject c = QJsonDocument::fromJson(arg(0).toUtf8()).object();
        if (c.contains("shareCursor")) shareCursor = c["shareCursor"].toBool();
        if (c.contains("syncMode")) { syncMode = c["syncMode"].toString(); r["switching"] = true; }
        if (c.contains("networks")) { syncMode = c["networks"].toString(); r["switching"] = true; }
        r["settings"] = QJsonObject{{"budget", c.contains("budget") ? c["budget"] : QJsonValue()}, {"shareCursor", shareCursor}, {"networks", syncMode}, {"syncMode", syncMode}};
    } else if (method == "bridge_note") {
        bridgeRequested = arg(1) == "true";
    } else if (method == "set_cursor") {
        lastCursor = arg(2) + "," + arg(3);
    } else if (method == "create_document") {
        QString id = QString("%1").arg(docs.size() + 1, 64, 10, QChar('0'));
        Doc d; d.text = arg(0).isEmpty() ? "" : arg(0) + "\n"; d.name = arg(0); docs[id] = d;
        r["id"] = id; r["current"] = detail(id, d);
    } else if (method == "join_document") {
        if (arg(0).size() != 64) return QJsonDocument(QJsonObject{{"ok", false}, {"error", "A note id is 64 hex characters"}}).toJson();
        docs[arg(0)]; r["id"] = arg(0); r["current"] = detail(arg(0), docs[arg(0)]);
    } else if (method == "set_text") {
        auto &d = docs[arg(0)];
        // Merge like the engine for the cases the harness drives: a stale base appends the
        // remote-only suffix (good enough to check the view's bookkeeping).
        QString sent = arg(2);
        bool stale = arg(1).toInt() < d.version && !d.text.isEmpty() && !sent.startsWith(d.text);
        QString merged = sent;
        if (stale && d.text.contains("REMOTE") && !sent.contains("REMOTE")) merged = sent + "REMOTE\n";
        d.version++; int ev = d.version; d.text = merged; d.name = merged.section('\n', 0, 0);
        if (merged != sent) d.version++;
        QJsonObject c = detail(arg(0), d); c["editVersion"] = QString::number(ev);
        r["current"] = c;
    } else if (method == "stats") {
        auto counts = [](int ops, int snaps, int heads, int rep, int pres) {
            return QJsonObject{{"ops", ops}, {"snapshots", snaps}, {"heads", heads}, {"repairs", rep}, {"relayed", 0},
                               {"presence", pres}, {"cursors", 0}, {"bytes", (ops + snaps + heads + rep + pres) * 700}};
        };
        QJsonObject names;
        for (auto it = docs.begin(); it != docs.end(); ++it) names[it.key()] = it->name.isEmpty() ? "Untitled" : it->name;
        QJsonArray topics;
        bool first = true;
        for (auto it = docs.begin(); it != docs.end(); ++it) {
            topics.append(QJsonObject{{"topic", it.key()}, {"active", first}, {"sent", counts(first ? 31 : 1, first ? 4 : 1, first ? 3 : 0, 1, first ? 6 : 0)},
                                      {"sent_this_epoch", counts(first ? 31 : 0, first ? 4 : 0, first ? 3 : 0, 1, first ? 6 : 0)}, {"received", counts(12, 2, 3, 0, 8)},
                                      {"peers", QJsonArray{QJsonObject{{"key", "ab12cd34ef56"}, {"seconds_ago", 4}}}}});
            first = false;
        }
        r["budget"] = 100; r["publishInterval"] = 1; r["cursorInterval"] = 1; r["snapshotQuiet"] = 10; r["names"] = names;
        r["settings"] = QJsonObject{{"budget", 100}, {"shareCursor", shareCursor}};
        r["stats"] = QJsonObject{{"transport", "logos-delivery"}, {"epoch_seconds", 600}, {"epoch_elapsed", 252}, {"uptime", 1500},
                                 {"sent", counts(40, 6, 5, 2, 9)}, {"sent_this_epoch", counts(31, 4, 3, 1, 6)}, {"sent_last_epoch", counts(12, 2, 3, 0, 5)},
                                 {"received", counts(20, 3, 4, 0, 10)}, {"topics", topics}};
    } else if (method == "remove_document") {
        docs.remove(arg(0));
    } else {
        return "";
    }
    return QString::fromUtf8(QJsonDocument(r).toJson(QJsonDocument::Compact));
}

void FakeCore::callModuleAsync(const QString &mod, const QString &method, const QVariant &args, const QJSValue &cb, int) {
    QString r = callModule(mod, method, args);
    QJSValue f = cb;
    QTimer::singleShot(delayMs, this, [f, r]() mutable { if (f.isCallable()) f.call(QJSValueList{QJSValue(r)}); });
}

static int failures = 0;
static void check(bool ok, const char *what) {
    fprintf(stderr, "%s %s\n", ok ? "CHECK-OK" : "CHECK-FAIL", what);
    if (!ok) failures++;
}
static QVariant js(QQuickView *v, const QString &code) {
    QQmlExpression e(qmlContext(v->rootObject()), v->rootObject(), code);
    QVariant r = e.evaluate();
    if (e.hasError()) fprintf(stderr, "[js-err] %s: %s\n", qPrintable(code), qPrintable(e.error().toString()));
    return r;
}
// Check `cond` (js) until it holds or `ms` pass, then run `next`.
static void eventually(QQuickView *v, const QString &cond, const char *what, int ms, std::function<void()> next) {
    auto start = std::make_shared<QElapsedTimer>();
    start->start();
    auto tick = std::make_shared<std::function<void()>>();
    *tick = [=]() {
        QQmlExpression e(qmlContext(v->rootObject()), v->rootObject(), cond);
        bool ok = e.evaluate().toBool();
        if (ok || start->elapsed() > ms) {
            fprintf(stderr, "%s %s (%lld ms)\n", ok ? "CHECK-OK" : "CHECK-FAIL", what, (long long)start->elapsed());
            if (!ok) failures++;
            next();
        } else {
            QTimer::singleShot(50, *tick);
        }
    };
    QTimer::singleShot(0, *tick);
}
static void grab(QQuickView *v, const QString &path) {
    QImage img = v->grabWindow();
    if (!img.isNull()) img.save(path);
    fprintf(stderr, "[shot] %s\n", qPrintable(path));
}

int main(int argc, char **argv) {
    QGuiApplication app(argc, argv);
    if (argc < 3) return 2;
    QString out = argv[2];
    auto *core = new FakeCore;
    QQuickView view;
    view.rootContext()->setContextProperty("logos", core);
    view.setResizeMode(QQuickView::SizeRootObjectToView);
    view.resize(1000, 680);
    QObject::connect(view.engine(), &QQmlEngine::warnings, [](const QList<QQmlError> &ws) {
        for (auto &w : ws) fprintf(stderr, "[qml-warn] %s\n", qPrintable(w.toString()));
    });
    view.setSource(QUrl::fromLocalFile(argv[1]));
    if (view.status() == QQuickView::Error) {
        for (auto &e : view.errors()) fprintf(stderr, "[load-err] %s\n", qPrintable(e.toString()));
        return 3;
    }
    view.show();
    QString id = QString("%1").arg(1, 64, 10, QChar('0'));
    auto at = [&](int ms, std::function<void()> f) { QTimer::singleShot(ms, f); };

    at(800, [&] { grab(&view, out + "/01-empty.png"); js(&view, "newTitle.text = 'Shopping'; createDoc()"); });
    at(1300, [&] {
        check(js(&view, "currentId").toString() == id, "create opens the new note");
        check(js(&view, "editor.text").toString() == "Shopping\n", "editor shows the new note");
        js(&view, "editor.cursorPosition = editor.text.length; editor.insert(editor.cursorPosition, '- milk\\n')");
    });
    at(2000, [&] {
        check(core->coreText(id) == "Shopping\n- milk\n", "typing is sent after the debounce");
        grab(&view, out + "/02-note.png");
        core->remoteSet(id, "Shopping\n- milk\n- eggs (from Bob)\n");
    });
    at(3600, [&] {
        check(js(&view, "editor.text").toString() == "Shopping\n- milk\n- eggs (from Bob)\n", "a remote change shows up");
        grab(&view, out + "/03-remote.png");
        // Typing while a reply is slow: the second edit must build on the first.
        core->setDelay(800);
        js(&view, "editor.insert(editor.text.length, 'a')");
    });
    at(4000, [&] { js(&view, "editor.insert(editor.text.length, 'b')"); });  // while the first is in flight
    at(6500, [&] {
        check(core->coreText(id).endsWith("ab"), "edits typed during a slow save are not lost");
        check(js(&view, "editor.text").toString() == core->coreText(id), "editor and core agree after a slow save");
        core->setDelay(0);
        js(&view, "joinField.text = 'nope'; joinDoc()");
    });
    at(7000, [&] {
        eventually(&view, "notice.indexOf('64 hex') >= 0", "a bad id shows the core's error", 2000, [] {});
        grab(&view, out + "/04-error.png");
        js(&view, "removeDoc(); removeDoc()");
    });
    at(7600, [&] {
        eventually(&view, "currentId === ''", "remove closes the note", 2000, [] {});
        check(!core->docs.contains(id), "remove reaches the core");
        grab(&view, out + "/05-removed.png");
        js(&view, "newTitle.text = '# Plan'; createDoc()");
    });
    at(7700, [&] {
        eventually(&view, "currentId !== '' && editor.text === '# Plan\\n'", "the # Plan note opens", 3000, [&] {
            js(&view, "editor.text = '# Plan\\n\\nSome **bold** and *italic* text, `code`, a [link](https://logos.co) and snake_case_name.\\n\\n## Tasks\\n- [x] done item\\n- [ ] open item with a long line that wraps around the edge of the editor so we can see the overlay wrap in the same place as the editor\\n1. first\\n> a quote with <html> & ampersands\\n\\n```\\nlet x = 1 < 2;\\n```\\n---\\nplain line'");
        });
    });
    at(8700, [&] {
        QString html = js(&view, "mdLayer.text").toString();
        check(js(&view, "mdLayer.visible").toBool(), "markdown colours are on");
        check(html.contains("&lt;html&gt; &amp; ampersands"), "markdown layer escapes html");
        check(!html.contains("snake<"), "snake_case is not emphasis");
        check(js(&view, "mdLayer.implicitHeight").toDouble() == js(&view, "editor.contentHeight").toDouble(), "markdown layer wraps like the editor");
        fprintf(stderr, "[heights] layer=%s editor=%s\n", qPrintable(js(&view, "mdLayer.implicitHeight").toString()), qPrintable(js(&view, "editor.contentHeight").toString()));
        grab(&view, out + "/06-markdown.png");
        // Alignment check: the editor's own text, in red, over the coloured layer.
        js(&view, "editor.color = 'red'");
    });
    at(9000, [&] {
        grab(&view, out + "/07-markdown-overlap.png");
        js(&view, "showStats = true");
        eventually(&view, "stats !== null && stats.ok === true", "stats panel loads the core's counters", 3000, [&] {
            check(js(&view, "statsPanel.used").toInt() == 45, "stats panel sums this epoch's messages");
            grab(&view, out + "/08-stats.png");
            js(&view, "showStats = false; editor.color = Qt.binding(function () { return root.mdOn ? 'transparent' : root.cText })");
            // Our own caret, visible over the coloured Markdown.
            js(&view, "editor.forceActiveFocus(); editor.cursorPosition = 6");
            QTimer::singleShot(300, [&] {
                QImage img = view.grabWindow();
                QPointF p = js(&view, "editor.mapToItem(null, editor.cursorRectangle.x + 1, editor.cursorRectangle.y + editor.cursorRectangle.height / 2)").toPointF();
                QColor c = img.pixelColor(p.toPoint());
                check(c.red() > 200 && c.green() < 160, "our caret is drawn (accent colour)");
                fprintf(stderr, "[caret] at %.0f,%.0f colour %s\n", p.x(), p.y(), qPrintable(c.name()));
                check(core->lastCursor.isEmpty(), "cursor not sent while sharing is off");
                js(&view, "shareHover.clicked(null)");
                eventually(&view, "settings.shareCursor === true", "share switch turns sharing on", 3000, [&] {
                    // A poll right after must not flip it back.
                    QTimer::singleShot(1500, [&] {
                        check(js(&view, "settings.shareCursor").toBool(), "sharing stays on after the next polls");
                        js(&view, "editor.cursorPosition = 8");
                        QTimer::singleShot(1200, [&] {
                            check(core->lastCursor == "8,8", "our cursor goes to the core once shared");
                            fprintf(stderr, "[cursor] %s\n", qPrintable(core->lastCursor));
                            // Someone else's cursor at offset 2 of "# Plan".
                            core_cursors = QJsonArray{QJsonObject{{"author", "ab12cd34ef56"}, {"pos", 2}, {"anchor", 2}}};
                            eventually(&view, "current && current.cursors && current.cursors.length === 1", "the view gets someone else's cursor", 3000, [&] {
                                QTimer::singleShot(200, [&] {
                                    QImage img = view.grabWindow();
                                    QPointF p = js(&view, "var r = editor.positionToRectangle(2); editor.mapToItem(null, r.x + 1, r.y + r.height / 2)").toPointF();
                                    QColor c = img.pixelColor(p.toPoint());
                                    QStringList tints{"#5fb3f9", "#f97fb3", "#9be37a", "#f9c45f", "#b48cf9", "#5fe0d0"};
                                    check(tints.contains(c.name()), "someone else's cursor is drawn in their colour");
                                    fprintf(stderr, "[remote] at %.0f,%.0f colour %s\n", p.x(), p.y(), qPrintable(c.name()));
                                    grab(&view, out + "/09-cursors.png");
                                    // The Networks choice: Both, then p2panda. No per-note bridge.
                                    grab(&view, out + "/10-networks-logos.png");
                                    js(&view, "setSettings({ networks: 'both' })");
                                    eventually(&view, "networks === 'both'", "the view follows the switch to both networks", 5000, [&] {
                                        QTimer::singleShot(500, [&] {
                                            QString st = js(&view, "statusText()").toString();
                                            check(st.contains("Logos: 7 peers") && st.contains("p2panda: 2 peers"), "the status shows both networks");
                                            fprintf(stderr, "[status] %s\n", qPrintable(st));
                                            grab(&view, out + "/11-networks-both.png");
                                            js(&view, "setSettings({ networks: 'p2panda' })");
                                            eventually(&view, "p2pandaMode === true", "the view follows the switch to p2panda", 5000, [&] {
                                                QTimer::singleShot(500, [&] {
                                                    check(!core->bridgeRequested, "no per-note bridge request");
                                                    grab(&view, out + "/12-p2panda.png");
                                                    fprintf(stderr, "[done] failures=%d calls=%d\n", failures, core->calls);
                                                    QGuiApplication::exit(failures);
                                                });
                                            });
                                        });
                                    });
                                });
                            });
                        });
                    });
                });
            });
        });
    });
    return app.exec();
}
#include "harness.moc"
