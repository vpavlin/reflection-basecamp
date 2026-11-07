#!/usr/bin/env bash
# Offscreen-render ../Main.qml with a fake reflection_core and check its behaviour.
# Design system 1.0.0 + Controls style Basic: trust it for "loads, no errors, behaves", not for
# exact layout (Basecamp bundles its own design-system build).
set -euo pipefail
QB=/nix/store/dkfr32yi7p8cdxsnll05q1kax19fl7ay-qtbase-6.9.2
QDECL=/nix/store/agvpq5n8vcqwnkmn8bp8rlczy3fdxm6n-qtdeclarative-6.9.2
DS=/nix/store/w9ra12n0yabd275v33m8x7lqnnrcgb9f-logos-design-system-1.0.0/lib
HERE="$(cd "$(dirname "$0")" && pwd)"; cd "$HERE"
"$QB/libexec/moc" harness.cpp -o harness.moc
g++ -std=c++17 -fPIC harness.cpp -o harness \
  -I"$QB/include" -I"$QB/include/QtCore" -I"$QB/include/QtGui" \
  -I"$QDECL/include" -I"$QDECL/include/QtQml" -I"$QDECL/include/QtQuick" \
  -L"$QB/lib" -L"$QDECL/lib" -lQt6Core -lQt6Gui -lQt6Qml -lQt6Quick
mkdir -p shots; rm -f shots/*.png
export QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software QT_QUICK_CONTROLS_STYLE=Basic
IMPORTS="$QDECL/lib/qt-6/qml:$DS"
export QML2_IMPORT_PATH="$IMPORTS" QML_IMPORT_PATH="$IMPORTS" LD_LIBRARY_PATH="$QB/lib:$QDECL/lib"
./harness "$HERE/../Main.qml" "$HERE/shots"
