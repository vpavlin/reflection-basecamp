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
#include <QImage>
#include <cstdio>

class FakeCore : public QObject {
    Q_OBJECT
public:
    struct Doc { QString name, text; int version = 1; int editVersion = 1; };
    QMap<QString, Doc> docs;
    int calls = 0;
    int delayMs = 0;
    Q_INVOKABLE QString callModule(const QString &mod, const QString &method, const QVariant &args);
    Q_INVOKABLE void callModuleAsync(const QString &mod, const QString &method, const QVariant &args, const QJSValue &cb, int timeoutMs);
    Q_INVOKABLE void remoteSet(const QString &id, const QString &text) { auto &d = docs[id]; d.text = text; d.version++; d.name = text.section('\n', 0, 0); }
    Q_INVOKABLE QString coreText(const QString &id) { return docs.value(id).text; }
    Q_INVOKABLE void setDelay(int ms) { delayMs = ms; }
};

static QJsonObject detail(const QString &id, const FakeCore::Doc &d) {
    return QJsonObject{{"id", id}, {"name", d.name.isEmpty() ? "Untitled" : d.name}, {"text", d.text},
                       {"version", QString::number(d.version)}, {"online", QJsonArray{"ab12"}}, {"subscribed", true}};
}

QString FakeCore::callModule(const QString &mod, const QString &method, const QVariant &v) {
    calls++;
    QVariantList a = v.toList();
    auto arg = [&](int i) { return i < a.size() ? a[i].toString() : QString(); };
    QJsonObject r{{"ok", true}};
    if (mod != "reflection_core") return "";
    if (method == "state") {
        QJsonArray list;
        for (auto it = docs.begin(); it != docs.end(); ++it)
            list.append(QJsonObject{{"id", it.key()}, {"name", it->name.isEmpty() ? "Untitled" : it->name}, {"online", 1}, {"updated", 0}, {"subscribed", true}});
        r["docs"] = list; r["node"] = "ready"; r["me"] = "cafe1234";
        r["transport"] = QJsonObject{{"status", "Connected"}, {"connected", true}, {"metrics", QJsonObject{{"peers", 6}}}};
        r["current"] = docs.contains(arg(0)) ? QJsonValue(detail(arg(0), docs[arg(0)])) : QJsonValue();
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
        r["budget"] = 100; r["publishInterval"] = 2; r["snapshotQuiet"] = 10; r["names"] = names;
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
        check(js(&view, "notice").toString().contains("64 hex"), "a bad id shows the core's error");
        grab(&view, out + "/04-error.png");
        js(&view, "removeDoc(); removeDoc()");
    });
    at(7600, [&] {
        check(js(&view, "currentId").toString().isEmpty(), "remove closes the note");
        check(!core->docs.contains(id), "remove reaches the core");
        grab(&view, out + "/05-removed.png");
        js(&view, "newTitle.text = '# Plan'; createDoc()");
    });
    at(8100, [&] {
        js(&view, "editor.text = '# Plan\\n\\nSome **bold** and *italic* text, `code`, a [link](https://logos.co) and snake_case_name.\\n\\n## Tasks\\n- [x] done item\\n- [ ] open item with a long line that wraps around the edge of the editor so we can see the overlay wrap in the same place as the editor\\n1. first\\n> a quote with <html> & ampersands\\n\\n```\\nlet x = 1 < 2;\\n```\\n---\\nplain line'");
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
    });
    at(9600, [&] {
        check(js(&view, "stats !== null && stats.ok").toBool(), "stats panel loads the core's counters");
        check(js(&view, "statsPanel.used").toInt() == 45, "stats panel sums this epoch's messages");
        grab(&view, out + "/08-stats.png");
        fprintf(stderr, "[done] failures=%d calls=%d\n", failures, core->calls);
        QGuiApplication::exit(failures);
    });
    return app.exec();
}
#include "harness.moc"
