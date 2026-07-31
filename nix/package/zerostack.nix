{ lib
, rustPlatform
, binutils
, mold
, openssl
, pkg-config
}:

let
  manifest = (lib.importTOML ../../Cargo.toml).package;
in
rustPlatform.buildRustPackage {
  pname = manifest.name;
  version = manifest.version;

  # TODO: upgrade to lib.fileset as cleanSource is including many irrelevent
  # files for the build (many *.md files, .git* files, & so on).
  src = lib.cleanSource ../..;

  cargoLock.lockFile = ../../Cargo.lock;

  nativeBuildInputs = [
    binutils
    mold
    pkg-config
  ];

  buildInputs = [
    openssl
  ];

  # Matches Cargo.toml default features plus optional extras.
  # js must be listed explicitly so feature drift in Cargo.toml doesn’t
  # silently drop the JS engine from the Nix package.
  buildFeatures = [
    "js"
    "acp"
    "memory"
    "multithread"
  ];

  # Network and subprocess tests cannot run inside the Nix sandbox.
  # The postInstall smoke below verifies the packaged binary instead.
  doCheck = false;

  postInstall = ‘’
    # Smoke the exact installed binary: version string and exit code.
    $out/bin/mini-agent --version | grep -Fq "mini-agent "
  ‘’;

  meta = {
    description = manifest.description;
    license = lib.licenses.gpl3Only;
    homepage = manifest.homepage;
    mainProgram = "mini-agent";
    platforms = with lib.platforms; linux ++ darwin;
  };
}
