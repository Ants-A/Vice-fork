{
  lib,
  rustPlatform,
  pkg-config,
  makeWrapper,
  wrapGAppsHook3,
  gtk3,
  gtk-layer-shell,
  libayatana-appindicator,
  webkitgtk_4_1,
  ffmpeg,
  gpu-screen-recorder,
  wf-recorder,
  wl-clipboard,
  xclip,
  xdotool,
  xprop,
  wmctrl,
  cloudflared,
  kdotool,
  extraRuntimePaths ? [ ],
}:
let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  runtimeDeps = [ ffmpeg gpu-screen-recorder wf-recorder wl-clipboard xclip xdotool xprop wmctrl cloudflared kdotool ] ++ extraRuntimePaths;
in
rustPlatform.buildRustPackage {
  pname = "vice-clipper";
  version = manifest.package.version;
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      lib.cleanSourceFilter path type &&
      !(builtins.elem (builtins.baseNameOf path) [ "target" "node_modules" "build" ".flatpak-cargo" ".flatpak-builder" ".pytest_cache" "__pycache__" "vice.egg-info" ]);
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ pkg-config makeWrapper wrapGAppsHook3 ];
  buildInputs = [ gtk3 webkitgtk_4_1 ];
  nativeCheckInputs = [ ffmpeg ];
  # Check fixtures and generated exports never touch the user's real library.
  preCheck = ''
    export VICE_DATA_DIR="$TMPDIR/vice-test-state"
    export VICE_CACHE_DIR="$TMPDIR/vice-test-cache"
    export VICE_CONFIG="$TMPDIR/vice-test-config.toml"
    mkdir -p "$VICE_DATA_DIR" "$VICE_CACHE_DIR"
  '';
  postInstall = ''
    mkdir -p $out/share/vice
    cp -r vice/ui vice/fonts vice/data $out/share/vice/
    install -Dm644 vice.desktop $out/share/applications/vice.desktop
    install -Dm644 assets/vice.svg $out/share/icons/hicolor/scalable/apps/vice.svg
    install -Dm644 packaging/vice.rules $out/lib/udev/rules.d/70-vice-input.rules
    install -Dm644 packaging/vice.service $out/lib/systemd/user/vice.service
    substituteInPlace $out/lib/systemd/user/vice.service \
      --replace-fail /usr/bin/vice $out/bin/vice
  '';
  preFixup = ''
    gappsWrapperArgs+=(--prefix PATH : ${lib.makeBinPath runtimeDeps})
    gappsWrapperArgs+=(--set VICE_RESOURCE_DIR $out/share/vice)
    gappsWrapperArgs+=(--prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ libayatana-appindicator gtk-layer-shell ]})
  '';
  meta = {
    description = "Instant-replay game clip recorder for Linux, for Wayland and X11";
    homepage = "https://github.com/splitshotz/Vice";
    license = lib.licenses.gpl3Plus;
    mainProgram = "vice-app";
    platforms = lib.platforms.linux;
  };
}
