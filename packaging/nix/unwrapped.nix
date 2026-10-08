{ lib
, rustPlatform
, cargo-tauri
, bun
, bun2nix
, pkg-config
, gtk3
, libsoup_3
, webkitgtk_4_1
, gtk-layer-shell
, tesseract
, leptonica
}:

rustPlatform.buildRustPackage {
  pname = "portunus-unwrapped";
  version = (lib.importTOML ../../src-tauri/Cargo.toml).package.version;

  # Only the inputs the build actually reads, so docs/CI/packaging commits do
  # not invalidate the build (and the binary cache).
  src = lib.fileset.toSource {
    root = ../../.;
    fileset = lib.fileset.unions [
      ../../src
      ../../src-tauri
      ../../extension-sdk # path dependency of the portunus crate
      ../../templates # scaffold templates, include_str!'d by cli_ext.rs
      ../../public
      ../../index.html
      ../../vite.config.ts
      ../../tsconfig.json
      ../../tsconfig.node.json
      ../../package.json
      ../../bun.lock
      ../../bun.nix
    ];
  };

  cargoRoot = "src-tauri";
  buildAndTestSubdir = "src-tauri";
  cargoLock.lockFile = ../../src-tauri/Cargo.lock;

  bunDeps = bun2nix.fetchBunDeps { bunNix = ../../bun.nix; };
  dontUseBunBuild = true; # cargo-tauri.hook owns build/install
  dontUseBunCheck = true;
  dontUseBunInstall = true;

  nativeBuildInputs = [
    cargo-tauri.hook
    bun2nix.hook
    bun
    pkg-config
    rustPlatform.bindgenHook # leptess' *-sys crates need libclang for bindgen
  ];

  buildInputs = [
    gtk3
    libsoup_3
    webkitgtk_4_1
    gtk-layer-shell
    tesseract
    leptonica
  ];

  doCheck = true;

  meta = {
    description = "Application launcher and power-user search for Wayland (portunus fork with fff file search)";
    homepage = "https://github.com/selfxplanatorium/portunus";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "portunus";
  };
}
