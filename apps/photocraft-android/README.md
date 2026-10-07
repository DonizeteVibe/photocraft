# PhotoCraft Android

This Gradle Android application packages the shared PhotoCraft egui UI in a NativeActivity APK.
The app module contains the Android manifest and the small Java `NativeActivity` subclass, which
bridges the Android Storage Access Framework picker. The Rust crate and its build manifest live in
[`rust/`](rust/); Rust owns the app state, egui frame loop, input translation, and wgpu renderer.
The Java bridge copies the selected document into app-private storage for the Rust runner to import.
Gradle plugin and library versions are managed in [`gradle/libs.versions.toml`](gradle/libs.versions.toml).

## Build

Install Java 17 or newer, the Android SDK/NDK, the Rust Android target, and cargo-ndk once:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

Set the SDK and NDK paths, then build the arm64 debug APK with Gradle:

```sh
cd apps/photocraft-android
ANDROID_HOME="$HOME/Library/Android/sdk" \
ANDROID_NDK_HOME="$HOME/Library/Android/sdk/ndk/28.2.13676358" \
./gradlew assembleDebug
```

Gradle invokes `cargo ndk` to build the Rust library, verifies the NativeActivity entry point, and
packages it with the Java platform bridge. The app compiles and targets API 36, supports API 26 and
later, and currently builds the `arm64-v8a` ABI. The APK is written to
`app/build/outputs/apk/debug/app-debug.apk`. The checked-in Gradle wrapper means no system Gradle
installation is needed.

## Current scope

The first target supports app launch, lifecycle/surface recreation, touch and basic key input, and
opening documents through the system picker. Save/export, Android clipboard, share intents, and a
touch-first layout remain future work.
