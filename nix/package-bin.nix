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
    x86_64 = "sha256-BO9DWHPvVDubujeRwGLN+cH0jk+N3wW6cLd2bO5cm7g=";
    aarch64 = "sha256-5USw+y76Vo3KUflr1RmbwD0LV0s45iab1z3v7rkux6k=";
  };
in
stdenv.mkDerivation rec {
  pname = "sommelier-rs-bin";
  version = "0.2.3";

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
