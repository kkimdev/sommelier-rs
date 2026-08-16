{
  autoPatchelfHook,
  fetchurl,
  lib,
  libgbm,
  libxkbcommon,
  stdenv,
}:

let
  targetCpu =
    {
      "x86_64-linux" = "x86_64";
      "aarch64-linux" = "aarch64";
    }
    .${stdenv.hostPlatform.system}
      or (throw "Unsupported system architecture: ${stdenv.hostPlatform.system}");

  hashMap = {
    x86_64 = "sha256-Y9qtH2k2ztnpOMMcOsSeVVXyZNAwedW5uUUj2Dg/uF4=";
    aarch64 = "sha256-2lhuQK5qxUkKUbH7x3pJxXYngFdhIDVTVM8WTZjr2vU=";
  };
in
stdenv.mkDerivation rec {
  pname = "sommelier-rs-bin";
  version = "0.2.5";

  src = fetchurl {
    url = "https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v${version}/sommelier_rs_virtwl-v${version}-${targetCpu}";
    hash = hashMap.${targetCpu};
  };

  dontUnpack = true;

  nativeBuildInputs = [ autoPatchelfHook ];
  buildInputs = [
    libgbm
    libxkbcommon
    stdenv.cc.cc.lib
  ];

  installPhase = ''
    runHook preInstall
    install -Dm755 "$src" "$out/bin/sommelier-rs"
    runHook postInstall
  '';

  meta = {
    description = "Sommelier-RS VirtWL proxy (prebuilt release binary)";
    homepage = "https://github.com/kkimdev/sommelier-rs";
    license = lib.licenses.asl20;
    mainProgram = "sommelier-rs";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
