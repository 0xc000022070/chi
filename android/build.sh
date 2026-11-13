#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

sdk=${ANDROID_HOME:?set ANDROID_HOME}
tools=$(ls -d "$sdk"/build-tools/* | sort -V | tail -1)
platform=$(ls -d "$sdk"/platforms/* | sort -V | tail -1)/android.jar
keys=${CHI_KEYS:-$HOME/.config/chi}
code=$(git rev-list --count HEAD)

rm -rf out && mkdir -p out/classes out/dex
"$tools/aapt2" compile --dir res -o out/res.zip
"$tools/aapt2" link -o out/unsigned.apk -I "$platform" --manifest AndroidManifest.xml --java out/gen \
    --version-code "$code" --version-name "$(git rev-parse --short HEAD)" out/res.zip
javac -nowarn -source 17 -target 17 -cp "$platform" -d out/classes out/gen/app/chi/R.java src/app/chi/Main.java
java -cp "$tools/lib/d8.jar" com.android.tools.r8.D8 --release --min-api 33 --lib "$platform" --output out/dex out/classes/app/chi/*.class
(cd out/dex && zip -q ../unsigned.apk classes.dex)
"$tools/zipalign" -f 4 out/unsigned.apk out/aligned.apk

if [ ! -f "$keys/android.jks" ]; then
    mkdir -p "$keys"
    (umask 077 && openssl rand -hex 24 > "$keys/android.pass")
    keytool -genkeypair -keystore "$keys/android.jks" -storepass:file "$keys/android.pass" \
        -alias chi -keyalg EC -validity 36500 -dname CN=chi
    chmod 600 "$keys/android.jks"
fi
java -jar "$tools/lib/apksigner.jar" sign --ks "$keys/android.jks" --ks-pass "file:$keys/android.pass" --out out/chi.apk out/aligned.apk
ls -l out/chi.apk

if [ "${1:-}" = publish ]; then
    CHI_TOKEN=$(cat "$keys/token-chi") chi push --url https://chi.nshard.com --version "$code" \
        --commit "$(git rev-parse HEAD)" --branch "$(git branch --show-current)" --tag release out/chi.apk
fi
