{
  lib,
  libgbm,
  libxkbcommon,
  pkg-config,
  rustPlatform,
  src,
}:

rustPlatform.buildRustPackage {
  pname = "sommelier-rs";
  version = "0.2.3";

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    libgbm
    libxkbcommon
  ];

  cargoBuildFlags = [
    "--package"
    "sommelier"
  ];
  cargoTestFlags = [
    "--package"
    "sommelier"
  ];
  checkFlags = [ "--test-threads=1" ];

  postInstall = ''
    ln -s sommelier "$out/bin/sommelier-rs"
  '';

  meta = {
    description = "Wayland proxy for ChromeOS VirtWL guests";
    homepage = "https://github.com/kkimdev/sommelier-rs";
    license = lib.licenses.asl20;
    mainProgram = "sommelier";
    platforms = lib.platforms.linux;
  };
}
